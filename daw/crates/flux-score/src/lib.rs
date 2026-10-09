//! Flux's composing toolkit: music theory, melody notation, voice leading, bass lines, phrasing, controller
//! shapes, and the score linter.
//!
//! The crate never changes a project. Its tools read a [`flux_core::Project`] and return
//! [`flux_core::Command`]s (to apply as one atomic, undoable batch) or a report, so whatever an agent or a
//! person does with them goes through the same command system as every other edit.
//!
//! # Layers
//!
//! * **Theory** ([`theory`], [`meter`]): note names, chord symbols (`Cmaj7`, `F#m7b5`, `G7(b9)/B`,
//!   `N.C.`), scales and modes, chord scales and how a pitch class sits over a chord (chord tone, tension,
//!   avoid note, outside), metric weight.
//! * **Building blocks**, ports of `daw/bridge/score.py` that give the same output for the same input:
//!   melody notation ([`notation`]), voice leading and bass lines ([`voicing`]), phrasing ([`phrase`]),
//!   ramps, curves and swells ([`shapes`]).
//! * **The chord track** ([`harmony`]): `Project::harmony` as time spans, and chord charts written as text
//!   (`"Am | F C | G@0 G/B@2.5 | %"`). Rhythm patterns for chords, bass lines and arpeggios ([`pattern`]).
//! * **The `compose` tool** ([`compose()`]): one JSON call writes a melody, voices chords, a bass line or an
//!   arpeggio over the chord track, shapes phrasing, draws a swell or a CC/pitch-bend curve, writes or reads
//!   the chord track, or checks (and fixes) a part against it. Re-running an action replaces what it wrote.
//! * **The `lint` tool** ([`lint()`](lint::lint)): clashes, strong-beat non-chord tones, ranges, mud,
//!   parallels, crossings, leaps, overlaps, controller pile-ups, data past the end, keyswitch hygiene and
//!   unreadable chord marks, each with a bar, a severity and a suggested fix.
//!
//! # Example
//!
//! ```
//! use flux_core::commands::apply;
//! use flux_score::lint::{Kind, LintOptions};
//! use serde_json::json;
//!
//! let mut project: flux_core::Project = serde_json::from_value(json!({
//!     "name": "demo", "tempo": [{"bar": 1, "bpm": 96.0}], "time_signature": [4, 4], "bars": 4, "key": "A minor",
//!     "tracks": [{"id": 1, "name": "Pad", "instrument": {"kind": "none"}},
//!                {"id": 2, "name": "Flute", "instrument": {"kind": "none"}}]
//! }))?;
//! // chords from a chart: the chart goes on the chord track, the voicings on the Pad
//! let out = flux_score::compose(&project, &json!({
//!     "action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 4, "chords": "Am | F | C | G"}))?;
//! for c in &out.commands {
//!     apply(&mut project, c)?;
//! }
//! // a melody over them
//! let out = flux_score::compose(&project, &json!({
//!     "action": "melody", "track": "Flute", "bar": 1, "notation": "E5:2 C5:2 | A4:4 | G5:2 E5:2 | D5:4"}))?;
//! for c in &out.commands {
//!     apply(&mut project, c)?;
//! }
//! let report = flux_score::lint(&project, &LintOptions::default());
//! assert_eq!(report.count(Kind::Clash), 0);
//! println!("{}", report.to_text(40));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Errors ([`Error`]) name the offending input and, for text, the character where the problem is.

pub mod compose;
pub mod error;
pub mod harmony;
pub mod lint;
pub mod meter;
pub mod notation;
pub mod pattern;
pub mod phrase;
pub mod shapes;
pub mod theory;
mod util;
pub mod voicing;

pub use compose::{ComposeOutput, compose, compose_schema, compose_tool};
pub use error::{ChordError, Error, KeyError, NoteError, Result, TextError};
pub use lint::{Finding, InstrumentCatalog, LintOptions, LintReport, lint, lint_handler, lint_schema, lint_tool};
