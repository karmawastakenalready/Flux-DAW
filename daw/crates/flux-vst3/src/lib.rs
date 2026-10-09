//! Native VST3 plugin hosting for Flux.
//!
//! * [`module`]: load a `.vst3` bundle, read its factory and classes (or `moduleinfo.json`).
//! * [`plugin`]: one instance in this process: component + controller (UI thread) and its
//!   [`plugin::Processor`] (audio thread), with state, parameters, programs and MIDI mapping.
//! * [`com`]: the COM objects a host provides to plugins (host context, streams, event lists...).
//!
//! Flux runs plugins out of process (see the `flux-plugin-host` binary) so a crashing or hanging
//! plugin can't take the DAW down; this crate holds both sides.

// VST3 SDK types differ by platform (TChar is u16 on Windows, char16_t elsewhere; enum widths vary), so
// casts that are no-ops on one platform are needed on another.
#![allow(clippy::unnecessary_cast)]

#[cfg(windows)]
pub mod client;
pub mod com;
#[cfg(windows)]
pub mod editor;
pub mod ipc;
pub mod module;
pub mod params;
pub mod plugin;
pub mod preset;
pub mod scan;
#[cfg(windows)]
pub mod server;
#[cfg(windows)]
pub mod settle;
#[cfg(windows)]
pub mod sys;
pub mod util;
pub mod vendor;

pub use module::{ClassInfo, FactoryInfo, Module};
pub use plugin::{Midi, ParamInfo, ParamPoint, Plugin, PluginInfo, Processor, ProgramList, Setup, State, TimedMidi, Transport};
