//! SFZ tooling for Flux: a parser and model that read SFZ files the way sfizz 1.2.3 does, the instrument
//! wrappers the songs use (built from set definitions, with a manifest so builds are incremental), the
//! instrument tools (tighten, retune, even, ranges, path checks, sample summaries), and a compact description
//! of an instrument for an agent.
//!
//! # Layers
//!
//! * [`syntax`]: the lossless document. [`Document::parse`] keeps every byte (comments, spacing, line ends,
//!   the encoding), so `to_bytes` gives the file back exactly; edits re-render only the items they touch.
//! * [`instrument`]: the model. [`Instrument::load`] follows `#include`s (relative to the root file's folder,
//!   cycles stopped), expands `#define` variables where they are used, applies `<global>`/`<master>`/`<group>`
//!   inheritance and `default_path`, and resolves each region's sample to an absolute path. [`RegionProps`]
//!   interprets the opcodes with sfizz's defaults, aliases and value parsing. Problems are [`Diagnostic`]s with
//!   `file:line:column`, never panics.
//! * [`edit`]: derived copies. The tools never change a library in place: they edit a [`edit::Derived`] copy
//!   that points back at the original samples, and the caller writes it ([`edit::write_files`]).
//! * [`set`], [`wrap`], [`manifest`]: the wrapper sets (`sfz/sets/<set>.toml`), wrapper generation, and the
//!   build/check that records every input and output by SHA-256.
//! * The tools: [`tighten`], [`retune`] (with [`pitch`]), [`even`] (renders through sfizz), [`ranges`],
//!   [`audit`] (missing samples, case mismatches), [`samples`] (with [`audio`]: WAV/FLAC headers and loops).
//! * [`info`]: everything an agent needs before writing a part, as JSON or a few lines of text.
//! * Front ends: [`cli`] (`flux sfz ...`), [`derive`] (the derived-copy tools behind one request, with the
//!   default output folder and the rule that `instruments/` is never written to) and [`catalog`] (programs
//!   versus fragments and one-line summaries for instrument listings, cached).
//!
//! # Example
//!
//! ```no_run
//! use flux_sfz::Instrument;
//!
//! let inst = Instrument::load("build/sfz/makima/violins.sfz")?;
//! for d in inst.errors() {
//!     eprintln!("{d}"); // file:line:col: error: sample not found ...
//! }
//! let info = flux_sfz::info::describe(&inst);
//! println!("{}", info.to_text());
//! let r = &info.ranges;
//! println!("plays {:?}, {} keyswitches", r.keys, r.keyswitches.len());
//! # Ok::<(), flux_sfz::Error>(())
//! ```

pub mod audio;
pub mod audit;
pub mod catalog;
pub mod cli;
pub mod derive;
pub mod edit;
pub mod error;
pub mod even;
pub mod fsutil;
pub mod info;
pub mod instrument;
pub mod manifest;
pub mod opcode;
pub mod pitch;
pub mod props;
pub mod ranges;
pub mod retune;
pub mod samples;
pub mod set;
pub mod syntax;
pub mod text;
pub mod tighten;
pub mod value;
pub mod wrap;

pub use error::{Diagnostic, Error, Location, Result, Severity};
pub use instrument::{Block, Control, Include, Instrument, Level, LoadOptions, Opcode, Region, SampleFile, SampleRef, SourceFile};
pub use props::RegionProps;
pub use syntax::{Document, Item, ItemKind};
