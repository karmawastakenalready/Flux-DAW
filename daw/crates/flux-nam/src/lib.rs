//! Guitar and bass rigs: Neural Amp Modeler (`.nam`) inference, cabinet IRs and the rig insert chain.
//!
//! A rig (`*.rig.json`, see [`rig`]) is an SFZ source plus an insert chain of filters, gain, gate, compressor,
//! impulse-response convolution, NAM captures and plugins. This crate reproduces `daw/bridge/rig.py`:
//!
//! * [`Rig`]: the file format, validation and job-parameter overrides ([`Rig::with_overrides`]);
//! * [`source_events`]: the note, controller and bend events for the source, including the round-robin
//!   pre-roll, ready for any SFZ engine;
//! * [`RigChain`]: the insert chain as a streaming, allocation-free mono processor;
//! * [`nam`]: `.nam` model loading and inference (WaveNet, LSTM, Linear; A1 and A2 files);
//! * [`resample`], [`convolution`], [`filters`], [`dynamics`]: the building blocks.
//!
//! ```no_run
//! use flux_nam::{NoHooks, Rig, RigChain};
//! use serde_json::json;
//!
//! let rig = Rig::load("sfz/rigs/gtx_5150_L.rig.json", ".")?;
//! let params = [("nam.input_db", json!(3.0))];
//! let rig = rig.with_overrides(params.iter().map(|(k, v)| (*k, v)))?;
//! let mut chain = RigChain::new(&rig, 48000, 512, &mut NoHooks)?;
//! let mut block = vec![0.0f32; 512]; // mono output of the rig's SFZ source
//! chain.process(&mut block);
//! println!("latency: {} samples", chain.latency());
//! # Ok::<(), flux_nam::Error>(())
//! ```
//!
//! `vst3` stages are run by processors the engine supplies through [`StageHooks`]; this crate hosts no plugins.
//!
//! Filters, gain, gate and compressor reproduce pedalboard bit for bit, NAM inference reproduces PyTorch to float
//! rounding. Where a sample-rate conversion is needed (a NAM capture or an IR at another rate than the host), the
//! default [`ChainOptions`] use cleaner resamplers than `rig.py`; [`ChainOptions::reference`] reproduces it. The
//! crate README lists the measured differences.

mod error;
mod json;

pub mod chain;
pub mod convolution;
pub mod dynamics;
pub mod events;
pub mod filters;
pub mod nam;
pub mod resample;
pub mod rig;

pub use chain::{
    ChainOptions, MAX_BLOCK, MonoProcessor, NoHooks, RigChain, StageContext, StageHooks, StereoAdapter, StereoProcessor,
};
pub use convolution::IrResampling;
pub use error::{Error, Result};
pub use events::{Event, EventKind, JobBend, JobCc, JobNote, SourceEvents, source_events};
pub use nam::{NamModel, NamNet, NamStage, SubmodelChoice};
pub use resample::Quality;
pub use rig::{DynamicsSpec, FilterShape, FilterSpec, IrSpec, NamSpec, Rig, Source, Stage, StageKind, Vst3Spec};
