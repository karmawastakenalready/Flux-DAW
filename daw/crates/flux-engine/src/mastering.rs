//! The master's tone and image stages, between the glue compressor and the loudness drive, in their order: the EQ
//! (minimum or linear phase), the dynamic EQ, the de-esser, the multiband compressor, the exciter, the imager, mono
//! below and the width.
//!
//! Live playback runs them block by block as one [`Tone`]. The offline render runs the same processors over the
//! whole master ([`render`]), measuring the loud level each dynamic stage counts from at that stage's input first
//! ([`Levels`], which live playback gets back through `RenderHints`), and recording what a part of the master (a
//! layer, a track stem) needs to get exactly its share of the result ([`Recording`], [`replay`]).

use crate::dsp::{self, Chain, MonoBelow, Stereo};
use crate::fx::dyneq::{self, DynEq};
use crate::fx::exciter::{self, Exciter};
use crate::fx::imager::Imager;
use crate::fx::linphase::{self, LinearPhase};
use crate::fx::multiband::{self, MultibandComp};
use flux_core::{Channel, DynBand, EqBand, EqKind, EqPhase, Master};

/// The loud levels (dBFS) the master's dynamic stages count from, as the render measured them at each stage's input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Levels {
    /// Each dynamic EQ band's.
    pub dyn_eq: Vec<f64>,
    /// The de-esser's band's.
    pub deess: Option<f64>,
    /// Each multiband compressor band's.
    pub multiband: Vec<f64>,
    /// Each exciter band's.
    pub exciter: Vec<f64>,
}

/// The de-esser as the dynamic EQ band it runs as.
fn deess_bands(m: &Master) -> Vec<DynBand> {
    m.deess.iter().map(|d| d.band()).collect()
}

/// Whether the master EQ runs as linear phase: asked for, with bands for the linear-phase filter to run (without
/// any it is minimum phase, which costs no latency).
fn linear(m: &Master) -> bool {
    m.eq_phase == EqPhase::Linear && m.eq.iter().any(|b| !is_pre(b))
}

/// A band the linear-phase EQ leaves to a minimum-phase filter before it: a high-pass. It sits lower than a
/// linear-phase filter of this length can follow (a 20 Hz cut would need one many times longer, and more latency),
/// and the phase shift it makes down there is inaudible.
fn is_pre(b: &EqBand) -> bool {
    b.kind == EqKind::HighPass
}

/// The master EQ's bands split for linear phase: the high-passes (see [`is_pre`]) and the rest.
fn split_linear(m: &Master) -> (Vec<EqBand>, Vec<EqBand>) {
    m.eq.iter().partition(|b| is_pre(b))
}

/// The master EQ's magnitude response (dB) at `hz` on the mid and the side.
fn response(chain: &Chain, sr: f64) -> impl Fn(f64) -> (f64, f64) + '_ {
    let ms = chain.has_mid_side();
    move |hz| {
        if ms {
            (chain.magnitude_db(hz, sr, Channel::Mid), chain.magnitude_db(hz, sr, Channel::Side))
        } else {
            let db = chain.magnitude_db(hz, sr, Channel::Stereo);
            (db, db)
        }
    }
}

enum Eq {
    Minimum(Chain),
    /// The high-passes as minimum-phase filters (`pre`), then the linear-phase filter with the bands it was designed
    /// for: designing it again costs a millisecond or two and starts a crossfade, so it happens only when they change.
    Linear { pre: Chain, fir: Box<LinearPhase>, bands: Vec<EqBand> },
}

impl Eq {
    fn new(sr: f64, m: &Master) -> Eq {
        if linear(m) {
            let (pre, bands) = split_linear(m);
            let fir = Box::new(LinearPhase::new(sr, &response(&dsp::eq_chain(sr, &bands), sr)));
            Eq::Linear { pre: dsp::eq_chain(sr, &pre), fir, bands }
        } else {
            Eq::Minimum(dsp::eq_chain(sr, &m.eq))
        }
    }

    fn retune(&mut self, sr: f64, m: &Master) {
        match self {
            Eq::Minimum(c) if !linear(m) => c.retune(dsp::eq_chain(sr, &m.eq)),
            Eq::Linear { pre, fir, bands } if linear(m) => {
                let (hp, rest) = split_linear(m);
                pre.retune(dsp::eq_chain(sr, &hp));
                if *bands != rest {
                    fir.retune(&response(&dsp::eq_chain(sr, &rest), sr));
                    *bands = rest;
                }
            }
            _ => *self = Eq::new(sr, m),
        }
    }
}

/// A dynamic EQ for `bands`, kept (retuned) when there was one; none without bands.
fn dyn_state(old: Option<DynEq>, sr: f64, bands: &[DynBand], loud: &[f64]) -> Option<DynEq> {
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

/// The stages after the glue compressor and before the loudness drive, streaming (see the module docs).
pub struct Tone {
    sr: f64,
    eq: Eq,
    dyn_eq: Option<DynEq>,
    deess: Option<DynEq>,
    multiband: Option<MultibandComp>,
    exciter: Option<Exciter>,
    imager: Option<Imager>,
    mono: Option<MonoBelow>,
    width: f64,
}

impl Tone {
    /// `lv`: the loud levels the last render measured (a stage without them guesses, see each).
    pub fn new(sr: f64, m: &Master, lv: &Levels) -> Tone {
        let mut t = Tone {
            sr,
            eq: Eq::new(sr, m),
            dyn_eq: None,
            deess: None,
            multiband: None,
            exciter: None,
            imager: None,
            mono: None,
            width: 1.0,
        };
        t.set(m, lv);
        t
    }

    /// New settings or levels; each stage's filters and envelopes carry on where its shape stays.
    pub fn retune(&mut self, m: &Master, lv: &Levels) {
        self.eq.retune(self.sr, m);
        self.set(m, lv);
    }

    fn set(&mut self, m: &Master, lv: &Levels) {
        let sr = self.sr;
        self.dyn_eq = dyn_state(self.dyn_eq.take(), sr, &m.dyn_eq, &lv.dyn_eq);
        self.deess = dyn_state(self.deess.take(), sr, &deess_bands(m), lv.deess.as_slice());
        self.multiband = m.multiband.as_ref().map(|mb| match self.multiband.take() {
            Some(mut c) => {
                c.retune(mb, &lv.multiband);
                c
            }
            None => MultibandComp::new(sr, mb, &lv.multiband),
        });
        self.exciter = m.exciter.as_ref().map(|e| match self.exciter.take() {
            Some(mut x) => {
                x.retune(e, &lv.exciter);
                x
            }
            None => Exciter::new(sr, e, &lv.exciter),
        });
        self.imager = m.imager.as_ref().map(|im| match self.imager.take() {
            Some(mut x) => {
                x.retune(im);
                x
            }
            None => Imager::new(sr, im),
        });
        let hz = m.mono_below_hz;
        self.mono = (hz > 0.0).then(|| match self.mono.take() {
            Some(mut x) => {
                x.retune(sr, hz);
                x
            }
            None => MonoBelow::new(sr, hz),
        });
        self.width = m.width;
    }

    /// How far the output runs behind the input (samples): the linear-phase EQ's delay, if it is on.
    pub fn latency(&self) -> usize {
        match &self.eq {
            Eq::Minimum(_) => 0,
            Eq::Linear { fir, .. } => fir.latency(),
        }
    }

    /// In place, any block length.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        match &mut self.eq {
            Eq::Minimum(c) => c.process(l, r),
            Eq::Linear { pre, fir, .. } => {
                pre.process(l, r);
                fir.process(l, r);
            }
        }
        if let Some(d) = &mut self.dyn_eq {
            d.process(l, r, None);
        }
        if let Some(d) = &mut self.deess {
            d.process(l, r, None);
        }
        if let Some(c) = &mut self.multiband {
            c.process(l, r, None);
        }
        if let Some(x) = &mut self.exciter {
            x.process(l, r, None);
        }
        image(l, r, &mut self.imager, &mut self.mono, self.width);
    }
}

/// The imager, mono below and the width.
fn image(l: &mut [f32], r: &mut [f32], imager: &mut Option<Imager>, mono: &mut Option<MonoBelow>, width: f64) {
    if let Some(x) = imager {
        x.process(l, r);
    }
    if let Some(x) = mono {
        x.process(l, r);
    }
    if width != 1.0 {
        dsp::place_s(l, r, 0.0, width);
    }
}

/// What the render's run over the whole master keeps for the parts: each dynamic stage's gains at each sample, and
/// what the exciter added.
#[derive(Default)]
pub struct Recording {
    dyn_eq: Vec<Vec<f32>>,
    deess: Vec<Vec<f32>>,
    multiband: Vec<Vec<f32>>,
    exciter: exciter::Recording,
}

/// The EQ over a whole buffer from silence, without its delay.
fn eq_offline(buf: &mut Stereo, sr: f64, m: &Master) {
    if linear(m) {
        let (pre, bands) = split_linear(m);
        dsp::eq_chain(sr, &pre).process(&mut buf.0, &mut buf.1);
        linphase::apply(buf, sr, &response(&dsp::eq_chain(sr, &bands), sr));
    } else {
        dsp::eq_chain(sr, &m.eq).process(&mut buf.0, &mut buf.1);
    }
}

/// A dynamic EQ run over the whole buffer from silence, recording its gains when `parts`.
fn dyn_offline(buf: &mut Stereo, sr: f64, bands: &[DynBand], loud: &[f64], parts: bool) -> Vec<Vec<f32>> {
    let mut d = DynEq::new(sr, bands, loud);
    let mut gains = vec![vec![]; if parts { bands.len() } else { 0 }];
    d.process(&mut buf.0, &mut buf.1, parts.then_some(&mut gains[..]));
    gains
}

/// The whole master through the stages from silence (see the module docs): the loud levels each dynamic stage
/// counted from, and, with `parts`, what [`replay`] needs.
pub fn render(buf: &mut Stereo, sr: f64, m: &Master, parts: bool) -> (Levels, Recording) {
    let mut lv = Levels::default();
    let mut rec = Recording::default();
    eq_offline(buf, sr, m);
    if !m.dyn_eq.is_empty() {
        lv.dyn_eq = dyneq::band_loud_levels(buf, sr, &m.dyn_eq);
        rec.dyn_eq = dyn_offline(buf, sr, &m.dyn_eq, &lv.dyn_eq, parts);
    }
    let deess = deess_bands(m);
    if !deess.is_empty() {
        let loud = dyneq::band_loud_levels(buf, sr, &deess);
        lv.deess = loud.first().copied();
        rec.deess = dyn_offline(buf, sr, &deess, &loud, parts);
    }
    if let Some(mb) = &m.multiband {
        lv.multiband = multiband::band_loud_levels(buf, sr, mb);
        let mut comp = MultibandComp::new(sr, mb, &lv.multiband);
        let mut gains = vec![vec![]; if parts { comp.bands() } else { 0 }];
        comp.process(&mut buf.0, &mut buf.1, parts.then_some(&mut gains[..]));
        rec.multiband = gains;
    }
    if let Some(e) = &m.exciter {
        lv.exciter = exciter::band_loud_levels(buf, sr, e);
        Exciter::new(sr, e, &lv.exciter).process(&mut buf.0, &mut buf.1, parts.then_some(&mut rec.exciter));
    }
    let mut imager = m.imager.as_ref().map(|im| Imager::new(sr, im));
    let mut mono = (m.mono_below_hz > 0.0).then(|| MonoBelow::new(sr, m.mono_below_hz));
    image(&mut buf.0, &mut buf.1, &mut imager, &mut mono, m.width);
    (lv, rec)
}

/// A part of the master through the stages, given the loud levels and recording of [`render`]'s run over the whole
/// master (with `parts`, on a buffer as long as this one): the part's share of the result. The parts of a master
/// sum to it.
pub fn replay(buf: &mut Stereo, sr: f64, m: &Master, lv: &Levels, rec: &Recording) {
    eq_offline(buf, sr, m);
    if !m.dyn_eq.is_empty() {
        let gains: Vec<&[f32]> = rec.dyn_eq.iter().map(|v| v.as_slice()).collect();
        DynEq::new(sr, &m.dyn_eq, &lv.dyn_eq).replay(&mut buf.0, &mut buf.1, &gains);
    }
    let deess = deess_bands(m);
    if !deess.is_empty() {
        let gains: Vec<&[f32]> = rec.deess.iter().map(|v| v.as_slice()).collect();
        DynEq::new(sr, &deess, lv.deess.as_slice()).replay(&mut buf.0, &mut buf.1, &gains);
    }
    if let Some(mb) = &m.multiband {
        multiband::apply_band_gains(buf, sr, mb, &rec.multiband);
    }
    if let Some(e) = &m.exciter {
        exciter::replay(buf, sr, e, &rec.exciter);
    }
    let mut imager = m.imager.as_ref().map(|im| Imager::new(sr, im));
    let mut mono = (m.mono_below_hz > 0.0).then(|| MonoBelow::new(sr, m.mono_below_hz));
    image(&mut buf.0, &mut buf.1, &mut imager, &mut mono, m.width);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{DeEss, EqBand, EqKind, Exciter as ExciterSpec, Imager as ImagerSpec, Multiband};
    use std::f64::consts::PI;

    const SR: f64 = 48000.0;

    fn noise(n: usize, amp: f32, mut seed: u64) -> Vec<f32> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 40) as f64 / (1u64 << 24) as f64 - 0.5) as f32 * amp
            })
            .collect()
    }

    fn tone(hz: f64, amp: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (amp * (2.0 * PI * hz * i as f64 / SR).sin()) as f32).collect()
    }

    /// A master with every stage on.
    fn everything(phase: EqPhase) -> Master {
        let mut side = EqBand::new(EqKind::HighShelf, 6000.0, 3.0, 0.7);
        side.channel = Channel::Side;
        let mut hp = EqBand::new(EqKind::HighPass, 30.0, 0.0, 0.7);
        hp.slope = 24;
        let mut dyn_mid = DeEss::default().band();
        dyn_mid.kind = EqKind::Peak;
        dyn_mid.hz = 2500.0;
        dyn_mid.channel = Channel::Mid;
        let mut mb = Multiband::default();
        mb.bands[1].channel = Channel::Mid;
        mb.bands[2].mix = 0.5;
        mb.bands[3].knee_db = 12.0;
        Master {
            eq: vec![hp, EqBand::new(EqKind::Peak, 300.0, -2.0, 1.0), side],
            eq_phase: phase,
            dyn_eq: vec![dyn_mid],
            deess: Some(DeEss::default()),
            multiband: Some(mb),
            exciter: Some(ExciterSpec::default()),
            imager: Some(ImagerSpec { crossovers_hz: vec![200.0, 4000.0], widths: vec![0.0, 1.0, 1.4] }),
            mono_below_hz: 90.0,
            width: 1.1,
            ..Master::default()
        }
    }

    fn signal(n: usize) -> (Stereo, Stereo) {
        let a: Vec<f32> = tone(110.0, 0.3, n).iter().zip(noise(n, 0.05, 1)).map(|(x, y)| x + y).collect();
        let a = (a.clone(), a.iter().zip(noise(n, 0.05, 2)).map(|(x, y)| x + y).collect());
        let b = (noise(n, 0.3, 3), noise(n, 0.3, 4));
        (a, b)
    }

    #[test]
    fn streaming_matches_the_render_after_the_latency() {
        let n = 96000;
        let (a, b) = signal(n);
        let x: Stereo = ((0..n).map(|i| a.0[i] + b.0[i]).collect(), (0..n).map(|i| a.1[i] + b.1[i]).collect());
        for phase in [EqPhase::Minimum, EqPhase::Linear] {
            let m = everything(phase);
            let mut whole = x.clone();
            let (lv, _) = render(&mut whole, SR, &m, false);
            let mut t = Tone::new(SR, &m, &lv);
            let lat = t.latency();
            assert_eq!(lat > 0, phase == EqPhase::Linear);
            let mut y = x.clone();
            for (l, r) in y.0.chunks_mut(480).zip(y.1.chunks_mut(480)) {
                t.process(l, r);
            }
            // (the dynamic stages hear the delay as silence first, so their envelopes start a little apart)
            let worst = (SR as usize / 2..n - lat).map(|i| (y.0[i + lat] - whole.0[i]).abs().max((y.1[i + lat] - whole.1[i]).abs())).fold(0f32, f32::max);
            assert!(worst < 1e-4, "{phase:?}: live off the render by {worst:e}");
        }
    }

    #[test]
    fn parts_replayed_sum_to_the_whole() {
        let n = 96000;
        let (a, b) = signal(n);
        let x: Stereo = ((0..n).map(|i| a.0[i] + b.0[i]).collect(), (0..n).map(|i| a.1[i] + b.1[i]).collect());
        for phase in [EqPhase::Minimum, EqPhase::Linear] {
            let m = everything(phase);
            let mut whole = x.clone();
            let (lv, rec) = render(&mut whole, SR, &m, true);
            let (mut pa, mut pb) = (a.clone(), b.clone());
            replay(&mut pa, SR, &m, &lv, &rec);
            replay(&mut pb, SR, &m, &lv, &rec);
            let worst = (0..n).map(|i| (pa.0[i] + pb.0[i] - whole.0[i]).abs().max((pa.1[i] + pb.1[i] - whole.1[i]).abs())).fold(0f32, f32::max);
            assert!(worst < 2e-4, "{phase:?}: parts off by {worst:e}");
        }
    }

    #[test]
    fn a_high_pass_cuts_as_deep_in_linear_phase_as_in_minimum_phase() {
        // 12 Hz rumble under a 1 kHz tone: the 25 Hz high-pass is a minimum-phase filter in either mode
        let n = 96000;
        let x: Vec<f32> = tone(12.0, 0.3, n).iter().zip(tone(1000.0, 0.1, n)).map(|(a, b)| a + b).collect();
        let mut hp = EqBand::new(EqKind::HighPass, 25.0, 0.0, 0.7);
        hp.slope = 24;
        let level = |phase: EqPhase| {
            let m = Master { eq: vec![hp, EqBand::new(EqKind::Peak, 1000.0, 3.0, 1.0)], eq_phase: phase, ..Master::default() };
            let mut y = (x.clone(), x.clone());
            render(&mut y, SR, &m, false);
            // the rumble's level over the second half: the 1 kHz tone averages out of a 12 Hz-wide window
            let w = (SR / 12.0) as usize;
            let low: Vec<f64> = y.0[n / 2..].chunks(w).map(|c| c.iter().map(|&v| v as f64).sum::<f64>() / c.len() as f64).collect();
            20.0 * (low.iter().map(|v| v * v).sum::<f64>() / low.len() as f64).sqrt().log10()
        };
        let (min, lin) = (level(EqPhase::Minimum), level(EqPhase::Linear));
        assert!(min < -30.0 && (min - lin).abs() < 1.0, "minimum {min:.1} dB, linear {lin:.1} dB");
    }

    #[test]
    fn a_master_without_the_new_stages_renders_as_before() {
        // the EQ, multiband and image the render always had, in the same order
        let n = 48000;
        let (a, _) = signal(n);
        let m = Master {
            eq: vec![EqBand::new(EqKind::LowShelf, 120.0, 2.0, 0.7)],
            multiband: Some(Multiband::default()),
            mono_below_hz: 100.0,
            width: 0.9,
            ..Master::default()
        };
        let mut new = a.clone();
        render(&mut new, SR, &m, false);
        let mut old = a.clone();
        dsp::apply_eq(&mut old, SR, &m.eq);
        let mb = m.multiband.as_ref().unwrap();
        let loud = multiband::band_loud_levels(&old, SR, mb);
        MultibandComp::new(SR, mb, &loud).process(&mut old.0, &mut old.1, None);
        MonoBelow::new(SR, 100.0).process(&mut old.0, &mut old.1);
        dsp::place(&mut old, 0.0, 0.9);
        assert_eq!(new, old);
    }
}
