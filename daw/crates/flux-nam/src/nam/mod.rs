//! Neural Amp Modeler (`.nam`) models: loading and streaming inference.
//!
//! ```no_run
//! use flux_nam::nam::{NamModel, SubmodelChoice};
//! let model = NamModel::load("instruments/nam/x/model_a2.nam", SubmodelChoice::Largest)?;
//! let mut net = model.instance();
//! let mut block = vec![0.0f32; 256]; // mono samples at model.sample_rate()
//! net.process(&mut block);
//! # Ok::<(), flux_nam::Error>(())
//! ```
//!
//! Inference reproduces the PyTorch models of the `neural-amp-modeler` package (the reference `rig.py` uses)
//! to float precision; see [`file`] for the supported formats. Processing is streaming and allocation-free, with
//! no latency of its own: output sample `t` depends on input samples up to `t`.

mod activation;
pub(crate) mod file;
mod kernel;
mod lstm;
mod stage;
mod wavenet;

pub use file::SubmodelChoice;
pub use stage::NamStage;
pub(crate) use stage::db_gain;

use crate::error::{Error, Result, read_json};
use file::{Arch, Parsed};
use kernel::{Conv, Init, MulAdd, NR, Plane};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// A loaded `.nam` model. Cheap to clone; create one streaming [`NamNet`] per signal with
/// [`NamModel::instance`].
#[derive(Clone, Debug)]
pub struct NamModel {
    parsed: Arc<Parsed>,
    label: String,
}

/// Which submodel of an A2 container a [`NamModel`] uses.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmodelInfo {
    /// Index in the file.
    pub index: usize,
    /// Its `max_value` (relative size, 1.0 = full).
    pub max_value: f64,
    /// `max_value` of every submodel, in file order.
    pub available: Vec<f64>,
}

impl NamModel {
    /// Load a `.nam` file.
    pub fn load(path: impl AsRef<Path>, choice: SubmodelChoice) -> Result<NamModel> {
        let path = path.as_ref();
        let v = read_json(path)?;
        NamModel::from_json(&v, &path.display().to_string(), choice)
    }

    /// Parse a `.nam` document. `label` names it in errors.
    pub fn from_json(v: &Value, label: &str, choice: SubmodelChoice) -> Result<NamModel> {
        if let SubmodelChoice::Size(x) = choice
            && !x.is_finite()
        {
            return Err(Error::model(label, format!("submodel size {x} is not a finite number")));
        }
        let parsed = file::parse(v, label, choice)?;
        Ok(NamModel { parsed: Arc::new(parsed), label: label.to_owned() })
    }

    /// Where the model came from.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The sample rate the model expects (48000 when the file does not say).
    pub fn sample_rate(&self) -> u32 {
        self.parsed.sample_rate
    }

    /// `"WaveNet"`, `"LSTM"` or `"Linear"`.
    pub fn architecture(&self) -> &'static str {
        match self.parsed.net {
            Arch::WaveNet(_) => "WaveNet",
            Arch::Lstm(_) => "LSTM",
            Arch::Linear(_) => "Linear",
        }
    }

    /// Format version written in the file, if any.
    pub fn version(&self) -> Option<&str> {
        self.parsed.version.as_deref()
    }

    /// Input samples each output sample depends on (1 for LSTMs, whose memory is unbounded).
    pub fn receptive_field(&self) -> usize {
        match &self.parsed.net {
            Arch::WaveNet(w) => w.receptive_field(),
            Arch::Lstm(_) => 1,
            Arch::Linear(c) => c.kernel,
        }
    }

    /// The chosen submodel, for A2 containers.
    pub fn submodel(&self) -> Option<SubmodelInfo> {
        self.parsed.submodel.as_ref().map(|(index, max_value, available)| SubmodelInfo {
            index: *index,
            max_value: *max_value,
            available: available.clone(),
        })
    }

    /// A new streaming instance, in its rest state (as after silence).
    pub fn instance(&self) -> NamNet {
        let kind = match &self.parsed.net {
            Arch::WaveNet(w) => Kind::WaveNet(Box::new(wavenet::WaveNet::new(w))),
            Arch::Lstm(w) => Kind::Lstm(lstm::Lstm::new(w)),
            Arch::Linear(c) => Kind::Linear(Linear::new(c)),
        };
        NamNet { kind }
    }
}

enum Kind {
    WaveNet(Box<wavenet::WaveNet>),
    Lstm(lstm::Lstm),
    Linear(Linear),
}

/// One streaming instance of a model (mono, at the model's sample rate).
pub struct NamNet {
    kind: Kind,
}

impl std::fmt::Debug for NamNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let k = match &self.kind {
            Kind::WaveNet(w) => format!("WaveNet (receptive field {})", w.receptive_field()),
            Kind::Lstm(_) => "LSTM".into(),
            Kind::Linear(_) => "Linear".into(),
        };
        f.debug_struct("NamNet").field("kind", &k).finish()
    }
}

impl NamNet {
    /// Replace the input samples in `buf` by the model's output. Any length; never allocates.
    pub fn process(&mut self, buf: &mut [f32]) {
        match &mut self.kind {
            Kind::WaveNet(w) => w.process(buf),
            Kind::Lstm(l) => l.process(buf),
            Kind::Linear(l) => l.process(buf),
        }
    }

    /// Return to the rest state. Never allocates.
    pub fn reset(&mut self) {
        match &mut self.kind {
            Kind::WaveNet(w) => w.reset(),
            Kind::Lstm(l) => l.reset(),
            Kind::Linear(l) => l.reset(),
        }
    }
}

/// The `Linear` architecture: an FIR filter with optional bias.
struct Linear {
    conv: Conv,
    input: Plane,
    out: Plane,
    fma: bool,
}

impl Linear {
    fn new(c: &file::ConvW) -> Linear {
        let conv = Conv::new(1, 1, c.kernel, 1, 1, &c.weight, c.bias.as_deref());
        Linear {
            input: Plane::new(1, conv.history(), wavenet::CHUNK),
            out: Plane::new(1, 0, wavenet::CHUNK),
            conv,
            fma: wavenet::fma_available(),
        }
    }

    fn reset(&mut self) {
        // zero input history is the padded start
        self.input.data.fill(0.0);
        self.input.pos = self.input.hist;
    }

    fn process(&mut self, buf: &mut [f32]) {
        for chunk in buf.chunks_mut(wavenet::CHUNK) {
            #[cfg(target_arch = "x86_64")]
            if self.fma {
                // SAFETY: `fma` is only true when the CPU supports AVX2 and FMA.
                unsafe { self.chunk_avx2(chunk) };
                continue;
            }
            self.chunk::<kernel::Separate>(chunk);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn chunk_avx2(&mut self, x: &mut [f32]) {
        self.chunk::<kernel::Fused>(x)
    }

    #[inline(always)]
    fn chunk<M: MulAdd>(&mut self, x: &mut [f32]) {
        let n = x.len();
        let cols = n.div_ceil(NR) * NR;
        self.input.begin(cols);
        let row = self.input.cols_mut(0, 0, cols);
        row[..n].copy_from_slice(x);
        row[n..].fill(0.0);
        self.conv.run::<M>(&self.input, &mut self.out, cols, Init::Bias);
        x.copy_from_slice(self.out.cols(0, 0, n));
        self.input.end(n);
    }
}
