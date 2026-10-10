//! The channel strip's built-in processing, which the offline render and live playback share: filters and EQ, gain,
//! drive, the amp, chorus, bitcrush, clip, post gain, gate, compressor, dynamic EQ, de-esser, delay and the strip's
//! reverb. Pre inserts run before it and post inserts after it, in their hosts. The end of the strip (the volume
//! lane, the duck, mono below, pan and width) follows the post inserts.
//!
//! An offline render can also record what the strip's nonlinear stages did (a [`Trace`]: each one's gain at each
//! sample, or per band) and replay it on a part of the strip's input. The linear stages run again on the part and the
//! others apply their recorded gains, so the parts going into a group bus come out of it summing to its output. That
//! keeps layer and track stems exact through buses. The amp runs again on each part, which is close but not exact.

use crate::dsp::{self, AmpState, Chain, ChorusState, DelayState, FilterKind, GateState, MonoBelow, Stereo};
use crate::fx::dynamics::Compressor;
use crate::fx::dyneq::{self, DynEq};
use crate::fx::reverb::StripReverb;
use crate::fx::saturate::{Saturator, Shape};
use flux_core::{DynBand, Mixer};

/// The loud levels (dBFS) that relative thresholds count from: at the compressor's input, of each dynamic EQ band
/// and of the de-esser's band.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loud {
    pub comp: Option<f64>,
    pub dyn_eq: Vec<f64>,
    pub deess: Option<f64>,
}

/// Until a render has measured them: a compressor's input, and a band of it.
const LOUD_GUESS_DB: f64 = -18.0;
const BAND_LOUD_GUESS_DB: f64 = -24.0;

/// What the nonlinear stages of one offline run did, in the order they ran.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    items: Vec<Item>,
    /// The loud levels the run measured.
    pub loud: Loud,
}

#[derive(Clone, Debug)]
enum Item {
    /// The gain at each sample (gate, compressor).
    Gain(Vec<f32>),
    /// Each channel's output over what the stage's linear part makes of the input (drive, clip, bitcrush).
    Ratio(Vec<f32>, Vec<f32>),
    /// Each band's gain at each sample (dynamic EQ, de-esser).
    Bands(Vec<Vec<f32>>),
}

/// How an offline run treats the strip's nonlinear stages.
pub enum Mode<'a> {
    Run,
    /// Run, keeping what they did.
    Record(&'a mut Trace),
    /// A part of a recorded run's input: the linear stages run on it, the others apply what they did then.
    Replay(&'a Trace),
    /// [`Mode::Replay`] without the delay and the reverb: the part's share of the dry master.
    ReplayDry(&'a Trace),
}

enum Tape<'a> {
    Off,
    Rec(&'a mut Vec<Item>),
    Play(std::slice::Iter<'a, Item>),
}

impl Tape<'_> {
    fn next(&mut self) -> Option<&Item> {
        match self {
            Tape::Play(it) => it.next(),
            _ => None,
        }
    }
}

pub struct Strip {
    sr: f64,
    m: Mixer,
    /// Offline: relative thresholds count from loud levels measured at each stage's input (whole buffers).
    measure: bool,
    /// Offline: the samples of the buffer that are the signal (the rest flushes the compressor's lookahead).
    span: usize,
    loud: Loud,
    filt: Chain,
    gain: f32,
    drive: Option<Saturator>,
    amp: Option<AmpState>,
    chorus: Option<ChorusState>,
    clip: Option<Saturator>,
    post: f32,
    gate: Option<GateState>,
    comp: Option<Compressor>,
    dyn_eq: Option<DynEq>,
    deess: Option<DynEq>,
    delay: Option<DelayState>,
    reverb: Option<StripReverb>,
    mono: Option<MonoBelow>,
}

impl Strip {
    /// Live: relative thresholds count from `loud` (what the last render measured). `loop_s`: the loop's length, so
    /// the chorus LFO fits whole cycles in it; `phase`: the LFO's sample index for the next sample processed.
    pub fn new(sr: f64, m: &Mixer, loud: &Loud, loop_s: Option<f64>, phase: i64) -> Strip {
        let mut s = Strip {
            sr,
            m: m.clone(),
            measure: false,
            span: 0,
            loud: Loud::default(),
            filt: Chain::default(),
            gain: dsp::db_to_lin(m.gain_db) as f32,
            drive: None,
            amp: None,
            chorus: None,
            clip: None,
            post: dsp::db_to_lin(m.post_gain_db) as f32,
            gate: None,
            comp: None,
            dyn_eq: None,
            deess: None,
            delay: None,
            reverb: None,
            mono: None,
        };
        s.update(m, loud, loop_s, phase);
        s
    }

    /// Offline, over whole buffers from silence ([`Strip::run`]). `lfo`: for looped renders, the buffer index where
    /// the loop starts and the loop's length, so LFO effects fit whole cycles in the loop.
    pub fn offline(sr: f64, m: &Mixer, lfo: Option<(usize, usize)>) -> Strip {
        let (loop_s, phase) = match lfo {
            Some((origin, l)) if l > 0 => (Some(l as f64 / sr), -(origin as i64)),
            _ => (None, 0),
        };
        let mut s = Strip::new(sr, m, &Loud::default(), loop_s, phase);
        s.measure = true;
        s
    }

    fn filters(sr: f64, m: &Mixer) -> Chain {
        let mut c = Chain::default();
        if m.hpf_hz > 0.0 {
            c.append(dsp::pass_filter(FilterKind::Highpass, sr, m.hpf_hz, m.hpf_slope));
        }
        if m.lpf_hz > 0.0 {
            c.append(dsp::pass_filter(FilterKind::Lowpass, sr, m.lpf_hz, m.lpf_slope));
        }
        c.append(dsp::eq_chain(sr, &m.eq));
        c
    }

    /// New mixer settings; effect memories (filter states, echoes, envelopes, reverb tails) carry on where they can.
    pub fn update(&mut self, m: &Mixer, loud: &Loud, loop_s: Option<f64>, phase: i64) {
        let sr = self.sr;
        self.filt.retune(Self::filters(sr, m));
        self.drive = saturator(self.drive.take(), sr, (m.drive_db > 0.0).then_some(Shape::Drive(m.drive_db)));
        self.amp = match (&m.amp, self.amp.take()) {
            (Some(a), Some(old)) if old.params() == a => Some(old),
            (Some(a), _) => Some(AmpState::new(sr, a)),
            _ => None,
        };
        self.chorus = match (&m.chorus, self.chorus.take()) {
            (Some(c), old) => {
                let rate = match loop_s {
                    Some(ls) if ls > 0.0 => (c.rate_hz * ls).round().max(1.0) / ls,
                    _ => c.rate_hz,
                };
                match old {
                    Some(o) if o.params().0 == c && o.params().1 == rate => Some(o),
                    _ => {
                        let mut s = ChorusState::new(sr, c, rate);
                        s.set_phase(phase);
                        Some(s)
                    }
                }
            }
            _ => None,
        };
        self.clip = saturator(self.clip.take(), sr, m.clip_db.map(Shape::Clip));
        self.gate = m.gate.map(|g| match self.gate.take() {
            Some(mut s) => {
                s.retune(sr, &g);
                s
            }
            None => GateState::new(sr, &g),
        });
        self.comp = m.comp.map(|c| {
            let thr = if c.relative { loud.comp.unwrap_or(LOUD_GUESS_DB) + c.threshold_db } else { c.threshold_db };
            match self.comp.take() {
                Some(mut s) => {
                    s.retune(sr, &c, thr);
                    s
                }
                None => Compressor::new(sr, &c, thr),
            }
        });
        let band_loud = |n: usize, have: &[f64]| (0..n).map(|i| have.get(i).copied().unwrap_or(BAND_LOUD_GUESS_DB)).collect::<Vec<_>>();
        self.dyn_eq = dyn_eq(self.dyn_eq.take(), sr, &m.dyn_eq, &band_loud(m.dyn_eq.len(), &loud.dyn_eq));
        let deess: Vec<DynBand> = m.deess.iter().map(|d| d.band()).collect();
        self.deess = dyn_eq(self.deess.take(), sr, &deess, &band_loud(deess.len(), loud.deess.as_slice()));
        self.delay = m.delay.map(|d| match self.delay.take() {
            Some(mut s) => {
                s.retune(sr, &d);
                s
            }
            None => DelayState::new(sr, &d),
        });
        self.reverb = m.reverb.as_ref().map(|fx| match self.reverb.take() {
            Some(mut s) => {
                s.retune(fx);
                s
            }
            None => StripReverb::new(sr, fx),
        });
        self.mono = (m.mono_below_hz > 0.0).then(|| match self.mono.take() {
            Some(mut s) => {
                s.retune(sr, m.mono_below_hz);
                s
            }
            None => MonoBelow::new(sr, m.mono_below_hz),
        });
        self.m = m.clone();
    }

    /// Samples the output runs behind the input (the compressor's lookahead).
    pub fn latency(&self) -> usize {
        self.comp.as_ref().map_or(0, Compressor::latency)
    }

    /// The loud levels an offline run measured.
    pub fn loud(&self) -> &Loud {
        &self.loud
    }

    /// Live: the LFO's sample index for the next sample processed (after a seek).
    pub fn set_phase(&mut self, phase: i64) {
        if let Some(c) = &mut self.chorus {
            c.set_phase(phase);
        }
    }

    /// Live: a block in place; the output lags the input by `latency()`.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        self.stages(l, r, &mut Tape::Off, false);
    }

    /// Live: mono below, from the end of the strip (after the volume lane and the duck).
    pub fn mono_below(&mut self, l: &mut [f32], r: &mut [f32]) {
        if let Some(m) = &mut self.mono {
            m.process(l, r);
        }
    }

    /// Offline: a whole buffer from silence, with the lookahead taken out (the output lines up with the input).
    pub fn run(&mut self, buf: &mut Stereo, mode: Mode) {
        let n = buf.0.len();
        let lat = self.latency();
        self.span = n;
        buf.0.resize(n + lat, 0.0);
        buf.1.resize(n + lat, 0.0);
        match mode {
            Mode::Run => self.stages(&mut buf.0, &mut buf.1, &mut Tape::Off, false),
            Mode::Record(trace) => {
                trace.items.clear();
                self.stages(&mut buf.0, &mut buf.1, &mut Tape::Rec(&mut trace.items), false);
                trace.loud = self.loud.clone();
            }
            Mode::Replay(trace) => self.stages(&mut buf.0, &mut buf.1, &mut Tape::Play(trace.items.iter()), false),
            Mode::ReplayDry(trace) => self.stages(&mut buf.0, &mut buf.1, &mut Tape::Play(trace.items.iter()), true),
        }
        buf.0.drain(..lat);
        buf.1.drain(..lat);
    }

    fn stages(&mut self, l: &mut [f32], r: &mut [f32], tape: &mut Tape, dry: bool) {
        let sr = self.sr;
        let play = matches!(tape, Tape::Play(_));
        self.filt.process(l, r);
        ramp(l, r, &mut self.gain, dsp::db_to_lin(self.m.gain_db) as f32);
        if let Some(s) = &mut self.drive {
            saturate(s, sr, l, r, tape);
        }
        if let Some(a) = &mut self.amp {
            a.process(l, r);
        }
        if let Some(c) = &mut self.chorus {
            c.process(l, r);
        }
        if self.m.bitcrush_bits > 0.0 {
            match tape {
                Tape::Off => dsp::bitcrush_s(l, r, self.m.bitcrush_bits),
                Tape::Rec(items) => {
                    let (xl, xr) = (l.to_vec(), r.to_vec());
                    dsp::bitcrush_s(l, r, self.m.bitcrush_bits);
                    items.push(Item::Ratio(ratio(l, &xl), ratio(r, &xr)));
                }
                Tape::Play(_) => apply_ratio(l, r, tape.next()),
            }
        }
        if let Some(s) = &mut self.clip {
            saturate(s, sr, l, r, tape);
        }
        ramp(l, r, &mut self.post, dsp::db_to_lin(self.m.post_gain_db) as f32);
        if let Some(g) = &mut self.gate {
            match tape {
                Tape::Off => g.process(l, r, None),
                Tape::Rec(items) => {
                    let mut gains = Vec::with_capacity(l.len());
                    g.process(l, r, Some(&mut gains));
                    items.push(Item::Gain(gains));
                }
                Tape::Play(_) => apply_gain(l, r, tape.next(), 0),
            }
        }
        if let (Some(c), Some(set)) = (&mut self.comp, self.m.comp) {
            if play {
                apply_gain(l, r, tape.next(), c.latency());
            } else {
                if self.measure && set.relative {
                    let loud = dsp::loud_level_s(&l[..self.span], &r[..self.span], sr);
                    self.loud.comp = Some(loud);
                    c.retune(sr, &set, loud + set.threshold_db);
                }
                match tape {
                    Tape::Rec(items) => {
                        let mut gains = Vec::with_capacity(l.len());
                        c.process(l, r, Some(&mut gains));
                        items.push(Item::Gain(gains));
                    }
                    _ => c.process(l, r, None),
                }
            }
        }
        if let Some(d) = &mut self.dyn_eq {
            if self.measure && !play && self.m.dyn_eq.iter().any(|b| b.relative) {
                let loud = dyneq::band_loud_levels(&(l[..self.span].to_vec(), r[..self.span].to_vec()), sr, &self.m.dyn_eq);
                d.retune(&self.m.dyn_eq, &loud);
                self.loud.dyn_eq = loud;
            }
            run_dyn_eq(d, self.m.dyn_eq.len(), l, r, tape);
        }
        if let Some(d) = &mut self.deess {
            if self.measure && !play {
                let band: Vec<DynBand> = self.m.deess.iter().map(|x| x.band()).collect();
                let loud = dyneq::band_loud_levels(&(l[..self.span].to_vec(), r[..self.span].to_vec()), sr, &band);
                d.retune(&band, &loud);
                self.loud.deess = loud.first().copied();
            }
            run_dyn_eq(d, 1, l, r, tape);
        }
        if dry {
            return;
        }
        if let Some(d) = &mut self.delay {
            d.process(l, r);
        }
        if let Some(v) = &mut self.reverb {
            v.process(l, r);
        }
    }
}

/// A saturator for `shape` (None: none), keeping `old`'s filter memories.
fn saturator(old: Option<Saturator>, sr: f64, shape: Option<Shape>) -> Option<Saturator> {
    let shape = shape?;
    Some(match old {
        Some(mut s) => {
            s.set(shape);
            s
        }
        None => Saturator::new(sr, shape),
    })
}

/// A dynamic EQ for `bands` (none: None), keeping `old`'s memories.
fn dyn_eq(old: Option<DynEq>, sr: f64, bands: &[DynBand], loud: &[f64]) -> Option<DynEq> {
    if bands.is_empty() {
        return None;
    }
    Some(match old {
        Some(mut d) => {
            d.retune(bands, loud);
            d
        }
        None => DynEq::new(sr, bands, loud),
    })
}

fn saturate(s: &mut Saturator, sr: f64, l: &mut [f32], r: &mut [f32], tape: &mut Tape) {
    match tape {
        Tape::Off => s.process(l, r),
        Tape::Rec(items) => {
            let (mut xl, mut xr) = (l.to_vec(), r.to_vec());
            Saturator::linear(sr, s.shape()).process(&mut xl, &mut xr);
            s.process(l, r);
            items.push(Item::Ratio(ratio(l, &xl), ratio(r, &xr)));
        }
        Tape::Play(_) => {
            Saturator::linear(sr, s.shape()).process(l, r);
            apply_ratio(l, r, tape.next());
        }
    }
}

fn run_dyn_eq(d: &mut DynEq, bands: usize, l: &mut [f32], r: &mut [f32], tape: &mut Tape) {
    match tape {
        Tape::Off => d.process(l, r, None),
        Tape::Rec(items) => {
            let mut gains = vec![vec![]; bands];
            d.process(l, r, Some(&mut gains));
            items.push(Item::Bands(gains));
        }
        Tape::Play(_) => {
            if let Some(Item::Bands(gains)) = tape.next() {
                let g: Vec<&[f32]> = gains.iter().map(Vec::as_slice).collect();
                d.replay(l, r, &g);
            } else {
                debug_assert!(false, "the trace doesn't match the strip");
            }
        }
    }
}

/// `y / x` at each sample, where `x` is what the stage's linear part made of the input (1 where that is silent).
fn ratio(y: &[f32], x: &[f32]) -> Vec<f32> {
    y.iter().zip(x).map(|(&y, &x)| if x.abs() < 1e-7 { 1.0 } else { (y / x).clamp(-4.0, 4.0) }).collect()
}

fn apply_ratio(l: &mut [f32], r: &mut [f32], item: Option<&Item>) {
    let Some(Item::Ratio(gl, gr)) = item else {
        debug_assert!(false, "the trace doesn't match the strip");
        return;
    };
    l.iter_mut().zip(gl).for_each(|(x, g)| *x *= g);
    r.iter_mut().zip(gr).for_each(|(x, g)| *x *= g);
}

/// A recorded gain on a whole buffer whose stage ran `lat` samples behind its input.
fn apply_gain(l: &mut [f32], r: &mut [f32], item: Option<&Item>, lat: usize) {
    let Some(Item::Gain(g)) = item else {
        debug_assert!(false, "the trace doesn't match the strip");
        return;
    };
    for x in [&mut *l, &mut *r] {
        if lat > 0 {
            x.copy_within(..x.len().saturating_sub(lat), lat);
            let n = lat.min(x.len());
            x[..n].fill(0.0);
        }
        x.iter_mut().zip(g).for_each(|(x, g)| *x *= g);
    }
}

/// Apply a gain that glides from `cur` to `target` across the block (no zipper noise on fader moves).
pub fn ramp(l: &mut [f32], r: &mut [f32], cur: &mut f32, target: f32) {
    let n = l.len();
    if n == 0 {
        return;
    }
    if (*cur - target).abs() < 1e-6 {
        if target != 1.0 {
            l.iter_mut().for_each(|x| *x *= target);
            r.iter_mut().for_each(|x| *x *= target);
        }
    } else {
        let step = (target - *cur) / n as f32;
        for i in 0..n {
            let g = *cur + step * (i + 1) as f32;
            l[i] *= g;
            r[i] *= g;
        }
    }
    *cur = target;
}
