//! The rig insert chain, ready to process audio: [`RigChain`].
//!
//! ```no_run
//! use flux_nam::{NoHooks, Rig, RigChain};
//! let rig = Rig::load("sfz/rigs/gtx_5150_L.rig.json", ".")?;
//! let mut chain = RigChain::new(&rig, 48000, 512, &mut NoHooks)?;
//! let mut block = vec![0.0f32; 512]; // the rig source's mono signal
//! chain.process(&mut block);
//! # Ok::<(), flux_nam::Error>(())
//! ```
//!
//! Stages run in order on mono `f32` blocks. Processing never allocates, locks or blocks; all buffers are
//! sized in [`RigChain::new`] from the maximum block length. Like `rig.py`, the output is cleaned with the
//! equivalent of `np.nan_to_num` (NaN becomes 0, infinities the largest finite values); the input is cleaned the
//! same way so one bad sample cannot poison the filters' state.
//!
//! `vst3` stages are not hosted here: the engine supplies their processors through [`StageHooks`].

use crate::convolution::{IrResampling, IrStage, load_ir};
use crate::dynamics::{Compressor, Gate};
use crate::error::{Error, Result};
use crate::filters::Iir;
use crate::nam::{NamModel, NamStage, SubmodelChoice, db_gain};
use crate::resample::Quality;
use crate::rig::{NamSpec, Rig, StageKind, Vst3Spec};
use std::path::Path;

/// Largest block length accepted by [`RigChain::new`].
pub const MAX_BLOCK: usize = 1 << 16;

/// A mono processor supplied by the host for a stage this crate does not implement (`vst3`).
///
/// `process` is called from the audio thread with blocks of at most the chain's `max_block` samples; it must
/// not allocate or block if the chain is used in real time.
pub trait MonoProcessor: Send {
    /// Process `buf` in place.
    fn process(&mut self, buf: &mut [f32]);
    /// Delay the processor adds, in samples (reported in [`RigChain::latency`]).
    fn latency(&self) -> usize {
        0
    }
    /// Clear all state (called by [`RigChain::reset`]).
    fn reset(&mut self) {}
}

/// A stereo processor, for plugins that need two channels.
pub trait StereoProcessor: Send {
    /// Process both channels in place (equal lengths).
    fn process(&mut self, left: &mut [f32], right: &mut [f32]);
    /// Delay in samples.
    fn latency(&self) -> usize {
        0
    }
    /// Clear all state.
    fn reset(&mut self) {}
}

/// Runs a [`StereoProcessor`] as `rig.py` runs a `vst3` stage: the mono signal on both channels in, the mean
/// of the two channels out.
pub struct StereoAdapter<P> {
    inner: P,
    right: Vec<f32>,
}

impl<P: StereoProcessor> StereoAdapter<P> {
    /// Wrap `inner` for blocks of at most `max_block` samples (longer blocks are split).
    pub fn new(inner: P, max_block: usize) -> StereoAdapter<P> {
        StereoAdapter { inner, right: vec![0.0; max_block.max(1)] }
    }

    /// The wrapped processor.
    pub fn inner(&self) -> &P {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut P {
        &mut self.inner
    }
}

impl<P: StereoProcessor> MonoProcessor for StereoAdapter<P> {
    fn process(&mut self, buf: &mut [f32]) {
        let max = self.right.len();
        for left in buf.chunks_mut(max) {
            let right = &mut self.right[..left.len()];
            right.copy_from_slice(left);
            self.inner.process(left, right);
            for (l, &r) in left.iter_mut().zip(right.iter()) {
                *l = (*l + r) * 0.5;
            }
        }
    }

    fn latency(&self) -> usize {
        self.inner.latency()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// What a hook gets to know about the stage it builds.
#[derive(Clone, Copy, Debug)]
pub struct StageContext<'a> {
    /// The stage's location for messages, e.g. `"sfz/rigs/x.rig.json: chain[3] (vst3)"`.
    pub at: &'a str,
    pub sample_rate: u32,
    pub max_block: usize,
    /// The rig's root directory (stage paths are already resolved against it).
    pub root: &'a Path,
}

/// Host services for building a chain: processors for `vst3` stages, and optionally `.nam` loading (to share
/// models between chains, for example).
pub trait StageHooks {
    /// A processor for a `vst3` stage. The default refuses: plugins are hosted by the engine.
    fn vst3(&mut self, spec: &Vst3Spec, ctx: &StageContext) -> anyhow::Result<Box<dyn MonoProcessor>> {
        let _ = ctx;
        anyhow::bail!("no VST3 host available to run {}", spec.path.display())
    }

    /// The model for a `nam` stage. The default loads the file.
    fn nam_model(&mut self, spec: &NamSpec, choice: SubmodelChoice) -> Result<NamModel> {
        NamModel::load(&spec.model, choice)
    }
}

/// Hooks that host nothing: chains with `vst3` stages fail to build with a clear error.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHooks;

impl StageHooks for NoHooks {}

/// Processing choices beyond what the rig file says.
///
/// The default favours quality; [`ChainOptions::reference`] reproduces `rig.py` (pedalboard and scipy) as
/// closely as this crate can. They differ only where a rate conversion is needed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChainOptions {
    /// Filter used when a NAM capture's rate differs from the processing rate. [`Quality::Scipy`] reproduces
    /// `rig.py` most closely; [`Quality::High`] (default) is cleaner.
    pub resample: Quality,
    /// How impulse responses recorded at another rate are resampled. [`IrResampling::Juce`] reproduces
    /// `rig.py`; [`IrResampling::High`] (default) keeps the IR's top octave intact.
    pub ir_resample: IrResampling,
    /// Overrides the submodel of every A2 (`SlimmableContainer`) capture; `None` uses the stage's `size`, or
    /// the largest submodel as `rig.py` does.
    pub submodel: Option<SubmodelChoice>,
}

impl ChainOptions {
    /// The options closest to `rig.py`: scipy's resampling filter for NAM captures, JUCE's resampler for
    /// impulse responses.
    pub fn reference() -> ChainOptions {
        ChainOptions { resample: Quality::Scipy, ir_resample: IrResampling::Juce, submodel: None }
    }
}

enum Proc {
    Filter(Iir),
    Gain(f32),
    Gate(Gate),
    Comp(Compressor),
    Ir(Box<IrStage>),
    Nam(Box<NamStage>),
    External(Box<dyn MonoProcessor>),
}

impl Proc {
    fn process(&mut self, buf: &mut [f32]) {
        match self {
            Proc::Filter(f) => f.process(buf),
            Proc::Gain(g) => buf.iter_mut().for_each(|v| *v *= *g),
            Proc::Gate(g) => g.process(buf),
            Proc::Comp(c) => c.process(buf),
            Proc::Ir(s) => s.process(buf),
            Proc::Nam(s) => s.process(buf),
            Proc::External(p) => p.process(buf),
        }
    }

    fn reset(&mut self) {
        match self {
            Proc::Filter(f) => f.reset(),
            Proc::Gain(_) => {}
            Proc::Gate(g) => g.reset(),
            Proc::Comp(c) => c.reset(),
            Proc::Ir(s) => s.reset(),
            Proc::Nam(s) => s.reset(),
            Proc::External(p) => p.reset(),
        }
    }

    fn latency(&self) -> usize {
        match self {
            Proc::Nam(s) => s.latency(),
            Proc::External(p) => p.latency(),
            _ => 0,
        }
    }
}

/// A rig's insert chain at a fixed sample rate.
pub struct RigChain {
    stages: Vec<(String, Proc)>,
    sample_rate: u32,
    max_block: usize,
}

impl std::fmt::Debug for RigChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stages: Vec<&str> = self.stages.iter().map(|(at, _)| at.as_str()).collect();
        f.debug_struct("RigChain")
            .field("stages", &stages)
            .field("sample_rate", &self.sample_rate)
            .field("max_block", &self.max_block)
            .field("latency", &self.latency())
            .finish()
    }
}

/// `np.nan_to_num` for `f32`.
#[inline]
fn clean(buf: &mut [f32]) {
    for v in buf {
        if !v.is_finite() {
            *v = if v.is_nan() {
                0.0
            } else if *v > 0.0 {
                f32::MAX
            } else {
                f32::MIN
            };
        }
    }
}

impl RigChain {
    /// Build the chain of `rig` for processing at `sample_rate` in blocks of at most `max_block` samples
    /// (longer calls are split internally). Loads the NAM models and impulse responses the rig names.
    pub fn new(rig: &Rig, sample_rate: u32, max_block: usize, hooks: &mut dyn StageHooks) -> Result<RigChain> {
        RigChain::with_options(rig, sample_rate, max_block, hooks, ChainOptions::default())
    }

    /// [`RigChain::new`] with explicit [`ChainOptions`].
    pub fn with_options(
        rig: &Rig,
        sample_rate: u32,
        max_block: usize,
        hooks: &mut dyn StageHooks,
        options: ChainOptions,
    ) -> Result<RigChain> {
        let at = format!("{}: chain", rig.label);
        if sample_rate == 0 {
            return Err(Error::stage(at, "sample rate must be positive"));
        }
        if max_block == 0 || max_block > MAX_BLOCK {
            return Err(Error::stage(at, format!("max_block {max_block} is outside 1..={MAX_BLOCK}")));
        }
        let sr = sample_rate as f64;
        let mut stages = Vec::with_capacity(rig.chain.len());
        for (i, st) in rig.chain.iter().enumerate() {
            let at = format!("{}: chain[{i}] ({})", rig.label, st.type_name());
            let proc = match &st.kind {
                StageKind::Filter(f) => Proc::Filter(Iir::new(f, sample_rate, &at)?),
                StageKind::Gain { db } => Proc::Gain(db_gain(*db)),
                StageKind::Gate(d) => Proc::Gate(Gate::new(d, sr)),
                StageKind::Comp(d) => Proc::Comp(Compressor::new(d, sr)),
                StageKind::Ir(s) => {
                    let ir = load_ir(&s.path, sample_rate, options.ir_resample, &at)?;
                    Proc::Ir(Box::new(IrStage::new(&ir, s.mix, max_block)))
                }
                StageKind::Nam(s) => {
                    let choice = options
                        .submodel
                        .unwrap_or_else(|| s.size.map_or(SubmodelChoice::Largest, SubmodelChoice::Size));
                    let model = hooks.nam_model(s, choice).map_err(|e| match e {
                        Error::Model { at: m, message } => Error::model(format!("{at}: {m}"), message),
                        other => other,
                    })?;
                    let stage = NamStage::new(&model, s.input_db, s.output_db, sample_rate, max_block, options.resample)
                        .map_err(|e| match e {
                            Error::Stage { message, .. } => Error::stage(at.clone(), message),
                            other => other,
                        })?;
                    Proc::Nam(Box::new(stage))
                }
                StageKind::Vst3(s) => {
                    let ctx = StageContext { at: &at, sample_rate, max_block, root: &rig.root };
                    let p = hooks.vst3(s, &ctx).map_err(|source| Error::Hook { at: at.clone(), source })?;
                    Proc::External(p)
                }
            };
            stages.push((at, proc));
        }
        Ok(RigChain { stages, sample_rate, max_block })
    }

    /// Process one block of the source's mono signal in place. Never allocates, locks or blocks (provided the
    /// hook-supplied processors do not).
    pub fn process(&mut self, buf: &mut [f32]) {
        for block in buf.chunks_mut(self.max_block) {
            clean(block);
            for (_, p) in &mut self.stages {
                p.process(block);
            }
            clean(block);
        }
    }

    /// Total delay of the chain in samples: output sample `t + latency()` corresponds to input sample `t`.
    pub fn latency(&self) -> usize {
        self.stages.iter().map(|(_, p)| p.latency()).sum()
    }

    /// Return every stage to its initial state (as after silence). Never allocates.
    pub fn reset(&mut self) {
        for (_, p) in &mut self.stages {
            p.reset();
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// Number of stages.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Each stage's location (as used in error messages) and latency in samples.
    pub fn stage_latencies(&self) -> impl Iterator<Item = (&str, usize)> {
        self.stages.iter().map(|(at, p)| (at.as_str(), p.latency()))
    }

    /// Process a whole signal offline, compensating the latency: the result is aligned with `input` and has
    /// its length, as `rig.py` returns it. Starts from the current state; allocates.
    pub fn process_offline(&mut self, input: &[f32]) -> Vec<f32> {
        let latency = self.latency();
        let mut buf = Vec::with_capacity(input.len() + latency);
        buf.extend_from_slice(input);
        buf.resize(input.len() + latency, 0.0);
        self.process(&mut buf);
        buf.drain(..latency);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rig(chain: serde_json::Value) -> Rig {
        let v = json!({"source": {"sfz": "x.sfz"}, "chain": chain});
        Rig::parse(&v, ".", "t.rig.json").unwrap()
    }

    #[test]
    fn chain_is_send() {
        fn send<T: Send>() {}
        send::<RigChain>();
    }

    #[test]
    fn vst3_needs_a_hook() {
        let r = rig(json!([{"type": "vst3", "path": "fx.vst3"}]));
        let e = RigChain::new(&r, 48000, 64, &mut NoHooks).unwrap_err().to_string();
        assert!(e.starts_with("t.rig.json: chain[0] (vst3): no VST3 host available"), "{e}");

        struct Halve;
        impl StereoProcessor for Halve {
            fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
                left.iter_mut().for_each(|v| *v *= 0.5);
                right.iter_mut().for_each(|v| *v = 0.0);
            }
            fn latency(&self) -> usize {
                3
            }
        }
        struct Host;
        impl StageHooks for Host {
            fn vst3(&mut self, _: &Vst3Spec, ctx: &StageContext) -> anyhow::Result<Box<dyn MonoProcessor>> {
                Ok(Box::new(StereoAdapter::new(Halve, ctx.max_block)))
            }
        }
        let mut c = RigChain::new(&r, 48000, 4, &mut Host).unwrap();
        assert_eq!(c.latency(), 3);
        let mut x = vec![1.0f32; 10];
        c.process(&mut x);
        // (0.5 + 0) / 2
        assert!(x.iter().all(|&v| v == 0.25), "{x:?}");
    }

    #[test]
    fn gains_and_cleanup() {
        let r = rig(json!([{"type": "gain", "db": -6}, {"type": "gain", "db": 6}]));
        let mut c = RigChain::new(&r, 44100, 64, &mut NoHooks).unwrap();
        let mut x = vec![1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.5];
        c.process(&mut x);
        let g = db_gain(-6.0) * db_gain(6.0);
        assert_eq!(x[0], g);
        assert_eq!(x[1], 0.0);
        assert!(x[2] > 1e38 && x[3] < -1e38);
        assert_eq!(x[4], 0.5 * db_gain(-6.0) * db_gain(6.0));
    }

    #[test]
    fn errors_name_the_stage() {
        let r = rig(json!([{"type": "lowpass", "hz": 30000}]));
        let e = RigChain::new(&r, 44100, 64, &mut NoHooks).unwrap_err().to_string();
        assert!(e.starts_with("t.rig.json: chain[0] (lowpass).hz:"), "{e}");
        let r = rig(json!([{"type": "nam", "model": "does/not/exist.nam"}]));
        let e = RigChain::new(&r, 44100, 64, &mut NoHooks).unwrap_err().to_string();
        assert!(e.contains("exist.nam"), "{e}");
        let e = RigChain::new(&rig(json!([])), 44100, 0, &mut NoHooks).unwrap_err().to_string();
        assert!(e.contains("max_block 0"), "{e}");
    }
}
