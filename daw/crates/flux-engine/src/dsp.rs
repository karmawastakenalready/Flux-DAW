//! Mixer / master DSP on planar stereo f32 buffers.
//!
//! Every effect with memory is a streaming processor (`*State`, `Chain`): live playback runs them block by
//! block, and the whole-buffer functions (used by the offline render) run the same processors from silence,
//! so both paths share one implementation.

use flux_core::{AmpFx, Channel, ChorusFx, Comp, DelayFx, EqBand, EqKind, Gate, ReverbParams};
use std::collections::VecDeque;
use std::f64::consts::PI;

pub type Stereo = (Vec<f32>, Vec<f32>);

pub fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

pub fn lin_to_db(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

// ------------------------------------------------------------------ biquad (RBJ cookbook)
#[derive(Clone, Copy)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

#[derive(Clone, Copy)]
pub enum FilterKind {
    Lowpass,
    Highpass,
    Peak,
    LowShelf,
    HighShelf,
    /// Removes a band (`q` sets its width).
    Notch,
    /// Passes a band, 0 dB at its centre.
    Bandpass,
    /// Flat magnitude, phase turning through 180 degrees at `hz`.
    Allpass,
}

impl Biquad {
    pub fn new(kind: FilterKind, sr: f64, hz: f64, q: f64, gain_db: f64) -> Biquad {
        let f = hz.clamp(10.0, sr * 0.49);
        let w0 = 2.0 * PI * f / sr;
        let (sw, cw) = w0.sin_cos();
        let q = q.max(0.05);
        let alpha = sw / (2.0 * q);
        let a = 10f64.powf(gain_db / 40.0);
        let (b0, b1, b2, a0, a1, a2) = match kind {
            FilterKind::Lowpass => ((1.0 - cw) / 2.0, 1.0 - cw, (1.0 - cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            FilterKind::Highpass => ((1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            FilterKind::Peak => (1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a),
            FilterKind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cw + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                    a * ((a + 1.0) - (a - 1.0) * cw - s),
                    (a + 1.0) + (a - 1.0) * cw + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                    (a + 1.0) + (a - 1.0) * cw - s,
                )
            }
            FilterKind::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cw + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                    a * ((a + 1.0) + (a - 1.0) * cw - s),
                    (a + 1.0) - (a - 1.0) * cw + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cw),
                    (a + 1.0) - (a - 1.0) * cw - s,
                )
            }
            FilterKind::Notch => (1.0, -2.0 * cw, 1.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            FilterKind::Bandpass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            FilterKind::Allpass => (1.0 - alpha, -2.0 * cw, 1.0 + alpha, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
        };
        Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// The filter's gain at `hz` (dB).
    pub fn magnitude_db(&self, hz: f64, sr: f64) -> f64 {
        let w = 2.0 * PI * hz / sr;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).max(1e-30).log10()
    }

    #[inline]
    pub fn tick(&self, s: &mut BqState, x: f32) -> f32 {
        let i = x as f64;
        let o = self.b0 * i + s.z1;
        s.z1 = self.b1 * i - self.a1 * o + s.z2;
        s.z2 = self.b2 * i - self.a2 * o;
        o as f32
    }

    /// Filter a block, carrying the filter memory in `s`.
    pub fn run(&self, s: &mut BqState, x: &mut [f32]) {
        for v in x.iter_mut() {
            *v = self.tick(s, *v);
        }
    }

    /// Filter a whole buffer from silence.
    pub fn process(&self, x: &mut [f32]) {
        self.run(&mut BqState::default(), x);
    }
}

/// Biquad memory (transposed direct form II).
#[derive(Clone, Copy, Default)]
pub struct BqState {
    z1: f64,
    z2: f64,
}

/// A cascade of biquads with stereo memory. Each stage filters both channels, or only the mid or the side signal.
#[derive(Clone, Default)]
pub struct Chain {
    stages: Vec<(Biquad, [BqState; 2], Channel)>,
}

/// Left and right into mid and side, in place: `(L + R) / 2` and `(L - R) / 2`.
pub fn to_mid_side(l: &mut [f32], r: &mut [f32]) {
    for (a, b) in l.iter_mut().zip(r.iter_mut()) {
        let (m, s) = ((*a + *b) * 0.5, (*a - *b) * 0.5);
        *a = m;
        *b = s;
    }
}

/// Mid and side back into left and right, in place.
pub fn from_mid_side(m: &mut [f32], s: &mut [f32]) {
    for (a, b) in m.iter_mut().zip(s.iter_mut()) {
        let (l, r) = (*a + *b, *a - *b);
        *a = l;
        *b = r;
    }
}

impl Chain {
    pub fn push(&mut self, f: Biquad) {
        self.push_on(f, Channel::Stereo);
    }

    /// A stage that filters only `ch` of the signal.
    pub fn push_on(&mut self, f: Biquad, ch: Channel) {
        self.stages.push((f, [BqState::default(); 2], ch));
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Append another cascade's stages after this one's.
    pub fn append(&mut self, other: Chain) {
        self.stages.extend(other.stages);
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        // the signal stays in mid/side between neighbouring mid or side stages
        let mut ms = false;
        for (f, s, ch) in self.stages.iter_mut() {
            let want = !ch.is_stereo();
            if want != ms {
                if want { to_mid_side(l, r) } else { from_mid_side(l, r) }
                ms = want;
            }
            match ch {
                Channel::Stereo => {
                    f.run(&mut s[0], l);
                    f.run(&mut s[1], r);
                }
                Channel::Mid => f.run(&mut s[0], l),
                Channel::Side => f.run(&mut s[1], r),
            }
        }
        if ms {
            from_mid_side(l, r);
        }
    }

    /// One channel (`ch` 0 or 1) of the cascade, for chains whose stages all filter both channels.
    pub fn process_ch(&mut self, ch: usize, x: &mut [f32]) {
        for (f, s, _) in self.stages.iter_mut() {
            f.run(&mut s[ch], x);
        }
    }

    /// Take new coefficients but keep the memory when the shape is unchanged (no click on a knob move).
    pub fn retune(&mut self, new: Chain) {
        let same = new.stages.len() == self.stages.len() && self.stages.iter().zip(&new.stages).all(|(a, b)| a.2 == b.2);
        if same {
            for (a, b) in self.stages.iter_mut().zip(new.stages) {
                a.0 = b.0;
            }
        } else {
            *self = new;
        }
    }

    /// The cascade's gain at `hz` (dB) on `part` of the signal: the mid or the side (the stages on both channels and
    /// those on that part), or with `Channel::Stereo` the stages on both channels only.
    pub fn magnitude_db(&self, hz: f64, sr: f64, part: Channel) -> f64 {
        self.stages.iter().filter(|s| s.2.is_stereo() || s.2 == part).map(|(f, _, _)| f.magnitude_db(hz, sr)).sum()
    }

    /// Whether any stage filters only the mid or the side.
    pub fn has_mid_side(&self) -> bool {
        self.stages.iter().any(|s| !s.2.is_stereo())
    }
}

/// A Butterworth high- or low-pass (`kind`) falling `slope` dB per octave (12, 24, 36 or 48): `slope / 12` biquads
/// with the Butterworth Qs, so the corner stays at -3 dB whatever the slope.
pub fn pass_filter(kind: FilterKind, sr: f64, hz: f64, slope: u32) -> Chain {
    let order = 2 * (slope / 12).clamp(1, 4) as usize;
    let mut c = Chain::default();
    for i in 1..=order / 2 {
        let q = if order == 2 {
            0.707 // as it always was, so older projects render the same
        } else {
            1.0 / (2.0 * ((2 * i - 1) as f64 * PI / (2 * order) as f64).cos())
        };
        c.push(Biquad::new(kind, sr, hz, q, 0.0));
    }
    c
}

pub fn apply_filter(buf: &mut Stereo, f: Biquad) {
    f.process(&mut buf.0);
    f.process(&mut buf.1);
}

pub fn eq_chain(sr: f64, bands: &[EqBand]) -> Chain {
    let mut c = Chain::default();
    for b in bands {
        let ch = b.channel;
        let kind = match b.kind {
            EqKind::Peak => FilterKind::Peak,
            EqKind::LowShelf => FilterKind::LowShelf,
            EqKind::HighShelf => FilterKind::HighShelf,
            EqKind::Notch => FilterKind::Notch,
            // two gentle shelves meeting at 0 dB at the pivot
            EqKind::Tilt => {
                c.push_on(Biquad::new(FilterKind::LowShelf, sr, b.hz, TILT_Q, -b.db), ch);
                c.push_on(Biquad::new(FilterKind::HighShelf, sr, b.hz, TILT_Q, b.db), ch);
                continue;
            }
            EqKind::HighPass | EqKind::LowPass => {
                let kind = if b.kind == EqKind::HighPass { FilterKind::Highpass } else { FilterKind::Lowpass };
                for (f, _, _) in pass_filter(kind, sr, b.hz, b.slope).stages {
                    c.push_on(f, ch);
                }
                continue;
            }
        };
        let q = if matches!(b.kind, EqKind::Peak | EqKind::Notch) { b.q } else { b.q.max(0.5) };
        c.push_on(Biquad::new(kind, sr, b.hz, q, b.db), ch);
    }
    c
}

/// The slope of a tilt EQ's shelves: the gentlest the shelf law allows without a bump.
const TILT_Q: f64 = 0.5;

pub fn apply_eq(buf: &mut Stereo, sr: f64, bands: &[EqBand]) {
    eq_chain(sr, bands).process(&mut buf.0, &mut buf.1);
}

pub fn gain(buf: &mut Stereo, db: f64) {
    gain_s(&mut buf.0, &mut buf.1, db);
}

pub fn gain_s(l: &mut [f32], r: &mut [f32], db: f64) {
    if db == 0.0 {
        return;
    }
    let g = db_to_lin(db) as f32;
    l.iter_mut().for_each(|x| *x *= g);
    r.iter_mut().for_each(|x| *x *= g);
}

/// tanh saturation (same law as pedalboard's Distortion: y = tanh(gain * x)).
pub fn drive(buf: &mut Stereo, drive_db: f64) {
    drive_s(&mut buf.0, &mut buf.1, drive_db);
}

pub fn drive_s(l: &mut [f32], r: &mut [f32], drive_db: f64) {
    if drive_db <= 0.0 {
        return;
    }
    let g = db_to_lin(drive_db) as f32;
    l.iter_mut().for_each(|x| *x = (*x * g).tanh());
    r.iter_mut().for_each(|x| *x = (*x * g).tanh());
}

// ------------------------------------------------------------------ guitar amp
const AMP_OS: usize = 4;

/// Guitar amp: pre-EQ, two saturation stages at 4x oversampling, cabinet filter, output level.
pub fn amp(buf: &mut Stereo, sr: f64, a: &AmpFx) {
    AmpState::new(sr, a).process(&mut buf.0, &mut buf.1);
}

pub struct AmpState {
    a: AmpFx,
    pre: Chain,
    aa1: Chain,
    mid: Chain,
    aa2: Chain,
    post: Chain,
    up: Vec<f32>,
}

impl AmpState {
    pub fn new(sr: f64, a: &AmpFx) -> AmpState {
        let osr = sr * AMP_OS as f64;
        // pre: tighten and push the mids before the gain, like a screamer in front of an amp
        let mut pre = Chain::default();
        pre.push(Biquad::new(FilterKind::Highpass, sr, a.tight_hz.max(20.0), 0.707, 0.0));
        if a.mid_db != 0.0 {
            pre.push(Biquad::new(FilterKind::Peak, sr, a.mid_hz.max(100.0), 0.8, a.mid_db));
        }
        // upsample (zero-stuff + lowpass), shape, lowpass, decimate
        let aa = || {
            let mut c = Chain::default();
            for q in [0.54, 1.31] {
                c.push(Biquad::new(FilterKind::Lowpass, osr, sr * 0.45, q, 0.0));
            }
            c
        };
        let mut mid = Chain::default();
        mid.push(Biquad::new(FilterKind::Highpass, osr, 30.0, 0.707, 0.0));
        mid.push(Biquad::new(FilterKind::Lowpass, osr, 9000.0, 0.707, 0.0));
        // cabinet
        let mut post = Chain::default();
        post.push(Biquad::new(FilterKind::Highpass, sr, a.cab_low_hz.max(20.0), 0.9, 0.0));
        post.push(Biquad::new(FilterKind::Peak, sr, a.cab_low_hz.max(20.0) * 1.6, 1.0, 2.5));
        if a.scoop_db != 0.0 {
            post.push(Biquad::new(FilterKind::Peak, sr, 450.0, 1.1, a.scoop_db));
        }
        if a.presence_db != 0.0 {
            post.push(Biquad::new(FilterKind::Peak, sr, 2800.0, 1.2, a.presence_db));
        }
        let hi = a.cab_high_hz.clamp(1000.0, sr * 0.45);
        post.push(Biquad::new(FilterKind::Lowpass, sr, hi, 0.75, 0.0));
        post.push(Biquad::new(FilterKind::Lowpass, sr, hi * 1.3, 0.6, 0.0));
        AmpState { a: *a, pre, aa1: aa(), mid, aa2: aa(), post, up: vec![] }
    }

    pub fn params(&self) -> &AmpFx {
        &self.a
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        self.pre.process(l, r);
        let pre_g = db_to_lin(self.a.gain_db) as f32;
        let lv = db_to_lin(self.a.level_db) as f32;
        // stage 1: biased soft clip (even harmonics), DC blocked; stage 2: harder symmetric clip
        let bias = 0.18f32;
        let off = bias.tanh();
        for (ci, ch) in [l, r].into_iter().enumerate() {
            let n = ch.len();
            self.up.clear();
            self.up.resize(n * AMP_OS, 0.0);
            for (i, x) in ch.iter().enumerate() {
                self.up[i * AMP_OS] = *x * AMP_OS as f32;
            }
            self.aa1.process_ch(ci, &mut self.up);
            for x in self.up.iter_mut() {
                *x = (*x * pre_g + bias).tanh() - off;
            }
            self.mid.process_ch(ci, &mut self.up);
            for x in self.up.iter_mut() {
                *x = (*x * 3.0).tanh();
            }
            self.aa2.process_ch(ci, &mut self.up);
            for i in 0..n {
                ch[i] = self.up[i * AMP_OS];
            }
            self.post.process_ch(ci, ch);
            ch.iter_mut().for_each(|x| *x *= lv);
        }
    }
}

// ------------------------------------------------------------------ chorus
/// Chorus with the JUCE/pedalboard law (mono LFO, delay = max(1 ms, centre + 20 ms * depth/2 * sin)), linear
/// dry/wet. `phase0` is the sample index where the LFO phase is zero (the loop start), so loops stay seamless.
pub fn chorus(buf: &mut Stereo, sr: f64, c: &ChorusFx, rate_hz: f64, phase0: usize) {
    if c.mix.clamp(0.0, 1.0) <= 0.0 {
        return;
    }
    let mut st = ChorusState::new(sr, c, rate_hz);
    st.set_phase(-(phase0 as i64));
    st.process(&mut buf.0, &mut buf.1);
}

pub struct ChorusState {
    c: ChorusFx,
    sr: f64,
    rate_hz: f64,
    lines: [Vec<f32>; 2],
    wr: usize,
    t: i64,
}

impl ChorusState {
    pub fn new(sr: f64, c: &ChorusFx, rate_hz: f64) -> ChorusState {
        let depth = c.depth.clamp(0.0, 1.0);
        let max_ms = c.centre_delay_ms.max(1.0) + 10.0 * depth + 2.0;
        let len = ((max_ms / 1000.0) * sr) as usize + 4;
        ChorusState { c: *c, sr, rate_hz, lines: [vec![0.0; len], vec![0.0; len]], wr: 0, t: 0 }
    }

    pub fn params(&self) -> (&ChorusFx, f64) {
        (&self.c, self.rate_hz)
    }

    /// Sample index, counted from the LFO's zero phase, of the next sample processed.
    pub fn set_phase(&mut self, t: i64) {
        self.t = t;
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let mix = self.c.mix.clamp(0.0, 1.0) as f32;
        if mix <= 0.0 {
            self.t += l.len() as i64;
            return;
        }
        let depth = self.c.depth.clamp(0.0, 1.0);
        let w = 2.0 * PI * self.rate_hz / self.sr;
        let len = self.lines[0].len();
        for i in 0..l.len() {
            let ph = w * self.t as f64;
            let d = (self.c.centre_delay_ms + 10.0 * depth * ph.sin()).max(1.0) / 1000.0 * self.sr;
            let di = d.floor() as usize;
            let fr = (d - di as f64) as f32;
            let (ia, ib) = ((self.wr + len - di) % len, (self.wr + len - di - 1) % len);
            let [la, lb] = &mut self.lines;
            for (x, line) in [(&mut l[i], la), (&mut r[i], lb)] {
                line[self.wr] = *x;
                let (a, b) = (line[ia], line[ib]);
                let wet = a + (b - a) * fr;
                *x = *x * (1.0 - mix) + wet * mix;
            }
            self.wr = (self.wr + 1) % len;
            self.t += 1;
        }
    }
}

/// Bit-depth reduction (pedalboard law: round(x * 2^bits) / 2^bits).
pub fn bitcrush(buf: &mut Stereo, bits: f64) {
    bitcrush_s(&mut buf.0, &mut buf.1, bits);
}

pub fn bitcrush_s(l: &mut [f32], r: &mut [f32], bits: f64) {
    if bits <= 0.0 {
        return;
    }
    let q = 2f64.powf(bits) as f32;
    l.iter_mut().for_each(|x| *x = (*x * q).round() / q);
    r.iter_mut().for_each(|x| *x = (*x * q).round() / q);
}

/// Hard clip at `db` dBFS.
pub fn clip(buf: &mut Stereo, db: f64) {
    clip_s(&mut buf.0, &mut buf.1, db);
}

pub fn clip_s(l: &mut [f32], r: &mut [f32], db: f64) {
    let t = db_to_lin(db) as f32;
    l.iter_mut().for_each(|x| *x = x.clamp(-t, t));
    r.iter_mut().for_each(|x| *x = x.clamp(-t, t));
}

/// 95th percentile of 400 ms short-term RMS over non-silent windows (dBFS): "how loud it gets".
pub fn loud_level_db(buf: &Stereo, sr: f64) -> f64 {
    loud_level_s(&buf.0, &buf.1, sr)
}

/// [`loud_level_db`] of two channels.
pub fn loud_level_s(l: &[f32], r: &[f32], sr: f64) -> f64 {
    let w = (0.4 * sr) as usize;
    let n = l.len().min(r.len()) / w.max(1);
    let mut v = vec![];
    for i in 0..n {
        let mut s = 0f64;
        for j in i * w..(i + 1) * w {
            let m = (l[j] as f64 + r[j] as f64) * 0.5;
            s += m * m;
        }
        let db = 10.0 * (s / w as f64 + 1e-24).log10();
        if db > -70.0 {
            v.push(db);
        }
    }
    if v.is_empty() {
        return -100.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * 0.95) as usize]
}

// ------------------------------------------------------------------ dynamics
/// The threshold a compressor works at: absolute, or relative to the loud level of `buf` (its input).
pub fn comp_threshold(buf: &Stereo, sr: f64, c: &Comp) -> f64 {
    if c.relative { loud_level_db(buf, sr) + c.threshold_db } else { c.threshold_db }
}

/// Feed-forward, stereo-linked peak compressor.
pub fn compress(buf: &mut Stereo, sr: f64, c: &Comp) {
    let thr = comp_threshold(buf, sr, c);
    CompState::new(sr, c, thr).process(&mut buf.0, &mut buf.1, None);
}

/// `compress`, also returning the threshold it used and the gain it applied per sample.
pub fn compress_env(buf: &mut Stereo, sr: f64, c: &Comp) -> (f64, Vec<f32>) {
    let thr = comp_threshold(buf, sr, c);
    let mut env = Vec::with_capacity(buf.0.len());
    CompState::new(sr, c, thr).process(&mut buf.0, &mut buf.1, Some(&mut env));
    (thr, env)
}

/// Streaming compressor at a resolved (absolute) threshold.
pub struct CompState {
    c: Comp,
    thr: f64,
    att: f64,
    rel: f64,
    env_db: f64,
}

impl CompState {
    pub fn new(sr: f64, c: &Comp, thr_db: f64) -> CompState {
        let mut s = CompState { c: *c, thr: thr_db, att: 0.0, rel: 0.0, env_db: -120.0 };
        s.retune(sr, c, thr_db);
        s
    }

    /// New settings; the envelope carries on.
    pub fn retune(&mut self, sr: f64, c: &Comp, thr_db: f64) {
        self.c = *c;
        self.thr = thr_db;
        self.att = (-1.0 / (c.attack_ms.max(0.05) * sr / 1000.0)).exp();
        self.rel = (-1.0 / (c.release_ms.max(1.0) * sr / 1000.0)).exp();
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], mut env: Option<&mut Vec<f32>>) {
        let ratio = self.c.ratio.max(1.0);
        let thr = self.thr;
        for i in 0..l.len() {
            let x = (l[i].abs().max(r[i].abs()) as f64).max(1e-9);
            let xdb = 20.0 * x.log10();
            self.env_db = if xdb > self.env_db {
                self.att * self.env_db + (1.0 - self.att) * xdb
            } else {
                self.rel * self.env_db + (1.0 - self.rel) * xdb
            };
            let gr = if self.env_db > thr { (thr + (self.env_db - thr) / ratio) - self.env_db } else { 0.0 };
            let g = db_to_lin(gr + self.c.makeup_db) as f32;
            l[i] *= g;
            r[i] *= g;
            if let Some(e) = env.as_deref_mut() {
                e.push(g);
            }
        }
    }
}

/// Noise gate: closes below the threshold (dBFS) with fast attack and a short release.
pub fn gate(buf: &mut Stereo, sr: f64, gt: &Gate) {
    GateState::new(sr, gt).process(&mut buf.0, &mut buf.1, None);
}

pub struct GateState {
    att: f64,
    rel: f64,
    det_rel: f64,
    thr: f64,
    env: f64,
    g: f64,
}

impl GateState {
    pub fn new(sr: f64, gt: &Gate) -> GateState {
        let mut s = GateState { att: 0.0, rel: 0.0, det_rel: 0.0, thr: 0.0, env: 0.0, g: 0.0 };
        s.retune(sr, gt);
        s
    }

    pub fn retune(&mut self, sr: f64, gt: &Gate) {
        self.att = (-1.0 / (gt.attack_ms.max(0.05) * sr / 1000.0)).exp();
        self.rel = (-1.0 / (gt.release_ms.max(0.5) * sr / 1000.0)).exp();
        self.det_rel = (-1.0 / (0.010 * sr)).exp();
        self.thr = db_to_lin(gt.threshold_db);
    }

    /// In place. `gains`: the gain applied at each sample is pushed.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], mut gains: Option<&mut Vec<f32>>) {
        for i in 0..l.len() {
            let x = l[i].abs().max(r[i].abs()) as f64;
            self.env = if x > self.env { x } else { self.det_rel * self.env + (1.0 - self.det_rel) * x };
            let target = if self.env > self.thr { 1.0 } else { 0.0 };
            self.g = if target > self.g {
                self.att * self.g + (1.0 - self.att) * target
            } else {
                self.rel * self.g + (1.0 - self.rel) * target
            };
            l[i] *= self.g as f32;
            r[i] *= self.g as f32;
            if let Some(g) = gains.as_deref_mut() {
                g.push(self.g as f32);
            }
        }
    }
}

// ------------------------------------------------------------------ delay, placement, ducking
pub fn delay(buf: &mut Stereo, sr: f64, d: &DelayFx) {
    DelayState::new(sr, d).process(&mut buf.0, &mut buf.1);
}

pub struct DelayState {
    d: DelayFx,
    lines: [Vec<f32>; 2],
    idx: usize,
}

impl DelayState {
    pub fn new(sr: f64, d: &DelayFx) -> DelayState {
        let n = ((d.seconds * sr) as usize).max(1);
        DelayState { d: *d, lines: [vec![0.0; n], vec![0.0; n]], idx: 0 }
    }

    /// New settings; the echoes survive unless the delay time changed.
    pub fn retune(&mut self, sr: f64, d: &DelayFx) {
        if ((d.seconds * sr) as usize).max(1) != self.lines[0].len() {
            *self = DelayState::new(sr, d);
        } else {
            self.d = *d;
        }
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let fb = self.d.feedback.clamp(0.0, 0.95) as f32;
        let mix = self.d.mix.clamp(0.0, 1.0) as f32;
        let n = self.lines[0].len();
        let mut end = self.idx;
        let [la, lb] = &mut self.lines;
        for (ch, line) in [(&mut *l, la), (&mut *r, lb)] {
            let mut idx = self.idx;
            for s in ch.iter_mut() {
                let out = line[idx];
                line[idx] = *s + out * fb;
                idx = (idx + 1) % n;
                *s = *s * (1.0 - mix) + out * mix;
            }
            end = idx;
        }
        self.idx = end;
    }
}

/// Mid/side width then constant-power balance.
/// Makes a signal mono below a frequency: its side `(L - R) / 2` through a 24 dB/octave high-pass, the mid untouched
/// (what a "bass mono" utility does: a wide low end folds down to mono without losing level).
#[derive(Clone, Default)]
pub struct MonoBelow {
    hp: Chain,
    side: Vec<f32>,
}

impl MonoBelow {
    pub fn new(sr: f64, hz: f64) -> MonoBelow {
        MonoBelow { hp: pass_filter(FilterKind::Highpass, sr, hz, 24), side: vec![] }
    }

    /// A new frequency; the filter's memory carries on.
    pub fn retune(&mut self, sr: f64, hz: f64) {
        self.hp.retune(pass_filter(FilterKind::Highpass, sr, hz, 24));
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        self.side.clear();
        self.side.extend(l.iter().zip(r.iter()).map(|(a, b)| (a - b) * 0.5));
        self.hp.process_ch(0, &mut self.side);
        for ((a, b), s) in l.iter_mut().zip(r.iter_mut()).zip(&self.side) {
            let m = (*a + *b) * 0.5;
            *a = m + s;
            *b = m - s;
        }
    }
}

pub fn place(buf: &mut Stereo, pan: f64, width: f64) {
    place_s(&mut buf.0, &mut buf.1, pan, width);
}

pub fn place_s(l: &mut [f32], r: &mut [f32], pan: f64, width: f64) {
    let th = (pan.clamp(-1.0, 1.0) + 1.0) * PI / 4.0;
    let gl = (2f64.sqrt() * th.cos()) as f32;
    let gr = (2f64.sqrt() * th.sin()) as f32;
    let w = width as f32;
    for i in 0..l.len() {
        let m = (l[i] + r[i]) * 0.5;
        let s = (l[i] - r[i]) * 0.5 * w;
        l[i] = (m + s) * gl;
        r[i] = (m - s) * gr;
    }
}

/// [`place_s`] with the pan of each sample from `pan`.
pub fn place_moving(l: &mut [f32], r: &mut [f32], pan: impl Fn(usize) -> f64, width: f64) {
    let w = width as f32;
    for i in 0..l.len() {
        let th = (pan(i).clamp(-1.0, 1.0) + 1.0) * PI / 4.0;
        let (gl, gr) = ((2f64.sqrt() * th.cos()) as f32, (2f64.sqrt() * th.sin()) as f32);
        let m = (l[i] + r[i]) * 0.5;
        let s = (l[i] - r[i]) * 0.5 * w;
        l[i] = (m + s) * gl;
        r[i] = (m - s) * gr;
    }
}

/// Sidechain-style gain envelope: dips by depth_db at each trigger (5 ms attack), exponential recovery.
pub fn duck_envelope(n: usize, sr: f64, triggers_s: &[f64], depth_db: f64, release_ms: f64) -> Vec<f32> {
    let mut d = vec![0f64; n];
    let tau = release_ms / 1000.0;
    let att = 0.005;
    for &tk in triggers_s {
        let a = ((tk - att) * sr).max(0.0) as usize;
        let b = (((tk + 6.0 * tau) * sr) as usize).min(n);
        for (i, di) in d.iter_mut().enumerate().take(b).skip(a) {
            let t = i as f64 / sr - tk;
            let v = duck_depth(t, depth_db, tau, att);
            if v > *di {
                *di = v;
            }
        }
    }
    d.into_iter().map(|x| db_to_lin(-x) as f32).collect()
}

/// Duck depth (dB) at `t` seconds after a trigger.
#[inline]
pub fn duck_depth(t: f64, depth_db: f64, tau: f64, att: f64) -> f64 {
    if t < 0.0 { depth_db * (1.0 + t / att) } else { depth_db * (-t / tau).exp() }
}

// ------------------------------------------------------------------ Freeverb
struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
}
struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

pub fn reverb(input: &Stereo, sr: f64, p: &ReverbParams) -> Stereo {
    let mut wet = (input.0.clone(), input.1.clone());
    ReverbState::new(sr, p).process(&mut wet.0, &mut wet.1);
    wet
}

/// Streaming Freeverb (mono in, stereo out) with pre-delay, return filters and level.
pub struct ReverbState {
    p: ReverbParams,
    sr: f64,
    tanks: [(Vec<Comb>, Vec<Allpass>); 2],
    pre: Vec<f32>,
    pre_idx: usize,
    filt: Chain,
}

impl ReverbState {
    pub fn new(sr: f64, p: &ReverbParams) -> ReverbState {
        const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
        const ALLP: [usize; 4] = [556, 441, 341, 225];
        let scale = sr / 44100.0;
        let tank = |spread: usize| {
            let combs: Vec<Comb> = COMBS
                .iter()
                .map(|&c| Comb { buf: vec![0.0; (((c + spread) as f64) * scale) as usize + 1], idx: 0, store: 0.0 })
                .collect();
            let alls: Vec<Allpass> =
                ALLP.iter().map(|&a| Allpass { buf: vec![0.0; (((a + spread) as f64) * scale) as usize + 1], idx: 0 }).collect();
            (combs, alls)
        };
        let pre = ((p.predelay_ms / 1000.0) * sr) as usize;
        let mut r = ReverbState { p: *p, sr, tanks: [tank(0), tank(23)], pre: vec![0.0; pre], pre_idx: 0, filt: Chain::default() };
        r.filt = r.filters();
        r
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

    pub fn params(&self) -> &ReverbParams {
        &self.p
    }

    /// New settings: the tail survives level / filter / tone changes; a new pre-delay restarts the tank.
    pub fn retune(&mut self, p: &ReverbParams) {
        if ((p.predelay_ms / 1000.0) * self.sr) as usize != self.pre.len() {
            *self = ReverbState::new(self.sr, p);
            return;
        }
        self.p = *p;
        let f = self.filters();
        self.filt.retune(f);
    }

    /// In place: the send signal in, the wet return out.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let room = (self.p.room_size.clamp(0.0, 1.0) * 0.28 + 0.7) as f32;
        let damp = (self.p.damping.clamp(0.0, 1.0) * 0.4) as f32;
        for i in 0..l.len() {
            let mono = (l[i] + r[i]) * 0.5;
            let src = if self.pre.is_empty() {
                mono
            } else {
                let o = self.pre[self.pre_idx];
                self.pre[self.pre_idx] = mono;
                self.pre_idx = (self.pre_idx + 1) % self.pre.len();
                o
            };
            let x = src * 0.015;
            for (ch, (combs, alls)) in self.tanks.iter_mut().enumerate() {
                let mut acc = 0f32;
                for c in combs.iter_mut() {
                    let o = c.buf[c.idx];
                    c.store = o * (1.0 - damp) + c.store * damp;
                    c.buf[c.idx] = x + c.store * room;
                    c.idx = (c.idx + 1) % c.buf.len();
                    acc += o;
                }
                for a in alls.iter_mut() {
                    let b = a.buf[a.idx];
                    let o = -acc + b;
                    a.buf[a.idx] = acc + b * 0.5;
                    a.idx = (a.idx + 1) % a.buf.len();
                    acc = o;
                }
                if ch == 0 {
                    l[i] = acc * 3.0;
                } else {
                    r[i] = acc * 3.0;
                }
            }
        }
        self.filt.process(l, r);
        gain_s(l, r, self.p.return_db);
    }
}

// ------------------------------------------------------------------ true-peak limiter
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Linked stereo peak per sample including 4x-oversampled inter-sample values (windowed-sinc interpolation).
pub fn oversampled_peaks(buf: &Stereo) -> Vec<f32> {
    const TAPS: i64 = 16;
    let n = buf.0.len();
    let mut h = [[0f64; (2 * TAPS) as usize]; 3];
    for k in 1..4 {
        let frac = k as f64 / 4.0;
        for (j, hj) in h[k - 1].iter_mut().enumerate() {
            let t = j as f64 - (TAPS - 1) as f64 - frac;
            let w = 0.5 + 0.5 * (PI * t / (TAPS as f64 + 1.0)).cos();
            *hj = sinc(t) * w;
        }
    }
    let mut out = vec![0f32; n];
    for (ci, ch) in [&buf.0, &buf.1].into_iter().enumerate() {
        for i in 0..n {
            let mut m = ch[i].abs() as f64;
            for hk in &h {
                let mut acc = 0f64;
                for (j, hj) in hk.iter().enumerate() {
                    let idx = i as i64 + j as i64 - (TAPS - 1);
                    if idx >= 0 && (idx as usize) < n {
                        acc += ch[idx as usize] as f64 * hj;
                    }
                }
                m = m.max(acc.abs());
            }
            if ci == 0 || m as f32 > out[i] {
                out[i] = m as f32;
            }
        }
    }
    out
}

fn sliding_min(x: &[f32], w: usize) -> Vec<f32> {
    let half = w / 2;
    let n = x.len();
    let mut out = vec![0f32; n];
    let mut dq: VecDeque<usize> = VecDeque::new();
    let mut next = 0usize;
    for i in 0..n {
        let hi = (i + half).min(n - 1);
        while next <= hi {
            while let Some(&b) = dq.back() {
                if x[b] >= x[next] {
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
        out[i] = x[*dq.front().unwrap()];
    }
    out
}

fn moving_average(x: &[f32], w: usize) -> Vec<f32> {
    let half = (w / 2) as i64;
    let n = x.len() as i64;
    let mut pre = vec![0f64; x.len() + 1];
    for i in 0..x.len() {
        pre[i + 1] = pre[i] + x[i] as f64;
    }
    (0..n)
        .map(|i| {
            let a = (i - half).max(0) as usize;
            let b = ((i + half + 1).min(n)) as usize;
            ((pre[b] - pre[a]) / (b - a) as f64) as f32
        })
        .collect()
}

/// Lookahead true-peak limiter: gain = min-filter of the required gain, smoothed by a moving average of
/// the same width (so the smoothed gain never exceeds what any peak needs). Returns max gain reduction dB.
pub fn tp_limit(buf: &mut Stereo, sr: f64, ceiling_db: f64, window_ms: f64) -> f64 {
    let ceil = db_to_lin(ceiling_db) as f32;
    let peaks = oversampled_peaks(buf);
    let req: Vec<f32> = peaks.iter().map(|&p| if p > ceil { ceil / p } else { 1.0 }).collect();
    let w = (((window_ms / 1000.0) * sr) as usize) | 1;
    let m = sliding_min(&req, w);
    let a = moving_average(&m, w);
    let mut min_g = 1f32;
    for i in 0..buf.0.len() {
        let g = a[i].min(m[i]);
        min_g = min_g.min(g);
        buf.0[i] *= g;
        buf.1[i] *= g;
    }
    lin_to_db(min_g as f64)
}

/// Streaming version of the limiter law above on sample peaks (live playback). The output lags the input
/// by `latency()` samples.
pub struct PeakLimiter {
    half: usize,
    ceil: f32,
    n: u64,
    delay: [VecDeque<f32>; 2],
    minq: VecDeque<(u64, f32)>,
    mins: VecDeque<f32>,
    sum: f64,
    /// Smallest gain since the last `take_min_gain`.
    pub min_gain: f32,
}

impl PeakLimiter {
    pub fn new(sr: f64, ceiling_db: f64, window_ms: f64) -> PeakLimiter {
        let w = (((window_ms / 1000.0) * sr) as usize) | 1;
        PeakLimiter {
            half: w / 2,
            ceil: db_to_lin(ceiling_db) as f32,
            n: 0,
            delay: [VecDeque::new(), VecDeque::new()],
            minq: VecDeque::new(),
            mins: VecDeque::new(),
            sum: 0.0,
            min_gain: 1.0,
        }
    }

    pub fn latency(&self) -> usize {
        2 * self.half
    }

    pub fn set_ceiling(&mut self, ceiling_db: f64) {
        self.ceil = db_to_lin(ceiling_db) as f32;
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let h = self.half as u64;
        let w = 2 * self.half + 1;
        for i in 0..l.len() {
            let pk = l[i].abs().max(r[i].abs());
            let req = if pk > self.ceil { self.ceil / pk } else { 1.0 };
            let n = self.n;
            while let Some(&(_, v)) = self.minq.back() {
                if v >= req {
                    self.minq.pop_back();
                } else {
                    break;
                }
            }
            self.minq.push_back((n, req));
            while let Some(&(k, _)) = self.minq.front() {
                if k + 2 * h < n {
                    self.minq.pop_front();
                } else {
                    break;
                }
            }
            // min over [n-2h, n] = the min-filter centred on n-h
            let m = self.minq.front().unwrap().1;
            self.mins.push_back(m);
            self.sum += m as f64;
            if self.mins.len() > w {
                self.sum -= self.mins.pop_front().unwrap() as f64;
            }
            self.delay[0].push_back(l[i]);
            self.delay[1].push_back(r[i]);
            if self.delay[0].len() > w {
                self.delay[0].pop_front();
                self.delay[1].pop_front();
            }
            if (n as usize) < 2 * self.half {
                l[i] = 0.0;
                r[i] = 0.0;
            } else {
                // output sample n-2h: average of the min-filter over [n-3h, n-h], capped by its own min
                let avg = (self.sum / self.mins.len() as f64) as f32;
                let own = self.mins[self.mins.len().saturating_sub(1 + self.half)];
                let g = avg.min(own);
                self.min_gain = self.min_gain.min(g);
                l[i] = self.delay[0][0] * g;
                r[i] = self.delay[1][0] * g;
            }
            self.n += 1;
        }
    }
}

/// TPDF dither (+/-1 LSB) at the final float -> 16-bit conversion. Interleaved output.
pub fn to_i16_tpdf(buf: &Stereo, seed: u64) -> Vec<i16> {
    let mut s = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut out = Vec::with_capacity(buf.0.len() * 2);
    for i in 0..buf.0.len() {
        for x in [buf.0[i], buf.1[i]] {
            let d = rnd() - rnd();
            let v = (x as f64 * 32767.0 + d).round().clamp(-32768.0, 32767.0);
            out.push(v as i16);
        }
    }
    out
}

#[cfg(test)]
mod mono_below_tests {
    use super::*;

    fn sine(hz: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * hz * i as f64 / 48000.0).sin() as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn the_side_goes_below_the_frequency_and_stays_above_it() {
        let n = 48000;
        // 60 Hz out of phase (all side), 2 kHz in phase (all mid), and 2 kHz out of phase at half level
        let (lo, hi) = (sine(60.0, n), sine(2000.0, n));
        let mut l: Vec<f32> = (0..n).map(|i| lo[i] + hi[i] + 0.5 * hi[i]).collect();
        let mut r: Vec<f32> = (0..n).map(|i| -lo[i] + hi[i] - 0.5 * hi[i]).collect();
        // in blocks, as the live engine runs it
        let mut m = MonoBelow::new(48000.0, 120.0);
        for (a, b) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
            m.process(a, b);
        }
        let tail = n / 2..n;
        let side: Vec<f32> = tail.clone().map(|i| (l[i] - r[i]) * 0.5).collect();
        let mid: Vec<f32> = tail.map(|i| (l[i] + r[i]) * 0.5).collect();
        // the mid is the 2 kHz tone, untouched
        assert!((rms(&mid) - 2f64.sqrt() / 2.0).abs() < 1e-3, "mid {}", rms(&mid));
        // the side is just the half-level 2 kHz: the 60 Hz is gone (24 dB/oct, an octave down)
        let want = 0.5 / 2f64.sqrt();
        assert!((rms(&side) / want - 1.0).abs() < 0.03, "side {} against {want}", rms(&side));
    }
}

#[cfg(test)]
mod pass_filter_tests {
    use super::*;

    /// Level of a sine at `hz` through a high-pass at 200 Hz of `slope` dB/octave, dB (after it settles).
    fn through(slope: u32, hz: f64) -> f64 {
        let sr = 48000.0;
        let n = 48000;
        let mut x: Vec<f32> = (0..n).map(|i| (2.0 * PI * hz * i as f64 / sr).sin() as f32).collect();
        let mut y = x.clone();
        pass_filter(FilterKind::Highpass, sr, 200.0, slope).process(&mut x, &mut y);
        let tail = &x[n / 2..];
        let rms = (tail.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        20.0 * (rms * 2f64.sqrt()).log10()
    }

    #[test]
    fn steeper_slopes_cut_more_below_the_corner_and_keep_it_at_minus_3_db() {
        for slope in [12, 24, 36, 48] {
            let corner = through(slope, 200.0);
            assert!((corner + 3.0).abs() < 0.1, "{slope} dB/oct: {corner:.2} dB at the corner");
            // two octaves down: about the slope times two
            let below = through(slope, 50.0);
            assert!((below + 2.0 * slope as f64).abs() < 1.0, "{slope} dB/oct: {below:.1} dB two octaves down");
            // the passband is untouched
            assert!(through(slope, 3200.0).abs() < 0.05);
        }
    }
}
