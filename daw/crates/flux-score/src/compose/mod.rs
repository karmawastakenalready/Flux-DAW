//! The `compose` tool: writes parts from the chord track and short text notations, as edit commands.
//!
//! [`compose`] never changes the project. It reads it and returns [`ComposeOutput`]: the
//! [`Command`]s to apply as one batch (one undo step, e.g. with `flux_core::Session::apply_batch`), a
//! compact JSON summary for the agent and warnings. Applying the commands to the same project state the
//! call saw gives the result the summary describes.
//!
//! | action     | writes                                                                               |
//! |------------|--------------------------------------------------------------------------------------|
//! | `melody`   | a line from notation (`E4:2 G4 r:1 | B4:4`), optionally phrased                       |
//! | `chords`   | voice-led chords from the chord track (or a chart), struck in a rhythm pattern         |
//! | `bass`     | a bass line from the chord track, in a rhythm pattern (root, octave, fifth)            |
//! | `arpeggio` | broken chords / ostinatos: voicing tones by index in a pattern                         |
//! | `phrase`   | re-shapes the dynamics (and optionally timing) of existing notes in a bar range        |
//! | `swell`    | an expression CC that follows a line, replacing that CC in the range                   |
//! | `curve`    | a CC or pitch-bend curve through points, replacing that data in its span               |
//! | `harmony`  | sets the chord track from a chart, or reads it back (tones, chord scales, avoid notes) |
//! | `fit`      | checks a part against the chord track; can move clashing notes to chord tones         |
//!
//! Re-running an action over the same range gives the same project: note-writing actions replace the
//! notes that start in their bars (keyswitch blips shorter than 0.05 beat are kept), `swell` and `curve`
//! replace the controller data they cover, and `phrase` starts from a base velocity when `vel` is given.

pub(crate) mod args;
mod chart;
mod control;
mod line;
mod parts;
mod schema;

pub use schema::{compose_schema, compose_tool};

use crate::error::{Error, Result};
use crate::harmony::ChordTrack;
use crate::meter::Meter;
use crate::theory::Scale;
use crate::theory::note::note_name_with;
use crate::util::{beat4, fmt_num};
use flux_core::pitch::PitchSpec;
use flux_core::commands::{NoteKey, NoteSpec};
use flux_core::{ChordMark, Command, Note, Project, Track, TrackRef};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Notes shorter than this (in beats) are keyswitch blips: replacing a range keeps them, and the fit check
/// and phrasing ignore them.
pub const BLIP_BEATS: f64 = 0.05;

/// The actions of [`compose`].
pub const ACTIONS: [&str; 9] = ["melody", "chords", "bass", "arpeggio", "phrase", "swell", "curve", "harmony", "fit"];

/// What a [`compose`] call produced.
#[derive(Clone, Debug, Serialize)]
pub struct ComposeOutput {
    /// The action that ran.
    pub action: String,
    /// Commands to apply in order as one atomic batch (one undo step). Empty for read-only calls
    /// (`harmony` without a chart, `fit` without `fix`).
    pub commands: Vec<Command>,
    /// What was written or found, for the agent.
    pub summary: Value,
    /// Things worth a look: parallel fifths, flagged notes, parts of the request that had no effect.
    pub warnings: Vec<String>,
    /// One line for the activity log, e.g. `compose chords: Pad bars 5-8`.
    pub label: String,
}

impl ComposeOutput {
    /// The tool result for the agent: action, summary and warnings, plus how many commands there are.
    /// (The caller applies [`ComposeOutput::commands`] and can add the new revision.)
    pub fn to_json(&self) -> Value {
        let mut v = json!({"action": self.action, "commands": self.commands.len(), "summary": self.summary});
        if !self.warnings.is_empty() {
            v["warnings"] = json!(self.warnings);
        }
        v
    }
}

/// Runs one compose action (`args["action"]`, the other fields as described by [`compose_schema`])
/// against `project`. Errors name the argument or the bar that is wrong; nothing is half-done.
///
/// ```
/// # use flux_core::{Project, Command, ChordMark};
/// # let mut project: Project = serde_json::from_value(serde_json::json!({
/// #     "name": "demo", "tempo": [{"bar": 1, "bpm": 90.0}], "time_signature": [4, 4], "bars": 8,
/// #     "key": "A minor", "tracks": []})).unwrap();
/// # flux_core::commands::apply(&mut project, &Command::AddTrack { name: "Pad".into(),
/// #     instrument: flux_core::Instrument::None, color: None }).unwrap();
/// let out = flux_score::compose::compose(&project, &serde_json::json!({
///     "action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 4,
///     "chords": "Am | F | C | G", "pattern": "x--x --x-", "step": 0.5,
/// })).unwrap();
/// // a SetHarmony (the chart goes on the chord track), then the notes
/// assert!(matches!(out.commands[0], Command::SetHarmony { .. }));
/// for c in &out.commands {
///     flux_core::commands::apply(&mut project, c).unwrap();
/// }
/// assert_eq!(project.harmony[0], ChordMark { bar: 1, beat: 0.0, chord: "Am".into() });
/// ```
pub fn compose(project: &Project, args: &Value) -> Result<ComposeOutput> {
    let obj = args.as_object().ok_or_else(|| Error::invalid("compose: the arguments must be a JSON object"))?;
    let action = match obj.get("action") {
        Some(Value::String(s)) => s.as_str(),
        Some(other) => return Err(Error::invalid(format!("compose: action must be a string (one of {}), got {other}", ACTIONS.join(", ")))),
        None => return Err(Error::invalid(format!("compose: \"action\" is required (one of {})", ACTIONS.join(", ")))),
    };
    let mut rest = obj.clone();
    rest.remove("action");
    let rest = Value::Object(rest);
    let mut cx = Ctx::new(project)?;
    let done = match action {
        "melody" => line::melody(&mut cx, parse(action, rest)?),
        "phrase" => line::phrase(&mut cx, parse(action, rest)?),
        "fit" => line::fit(&mut cx, parse(action, rest)?),
        "chords" => parts::chords(&mut cx, parse(action, rest)?),
        "bass" => parts::bass(&mut cx, parse(action, rest)?),
        "arpeggio" => parts::arpeggio(&mut cx, parse(action, rest)?),
        "swell" => control::swell(&mut cx, parse(action, rest)?),
        "curve" => control::curve(&mut cx, parse(action, rest)?),
        "harmony" => chart::harmony(&mut cx, parse(action, rest)?),
        other => return Err(Error::invalid(format!("compose: unknown action {other:?} (use one of {})", ACTIONS.join(", ")))),
    }
    .map_err(|e| match e {
        Error::Invalid(m) if !m.starts_with("compose ") => Error::Invalid(format!("compose {action}: {m}")),
        other => other,
    })?;
    Ok(ComposeOutput { action: action.to_string(), commands: done.commands, summary: done.summary, warnings: cx.warnings, label: format!("compose {action}: {}", done.label) })
}

fn parse<T: DeserializeOwned>(action: &str, v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error::invalid(format!("compose {action}: {e}")))
}

/// What an action returns to [`compose`].
pub(crate) struct Done {
    pub commands: Vec<Command>,
    pub summary: Value,
    pub label: String,
}

/// The project as an action sees it, and the warnings it collects.
pub(crate) struct Ctx<'a> {
    pub p: &'a Project,
    pub meter: Meter,
    pub bar_beats: f64,
    /// The project key, if it could be read.
    pub key: Option<Scale>,
    /// Why the project key could not be read (reported by the actions that use the key).
    key_problem: Option<String>,
    /// The chord track (after a chart given in this call, if any).
    pub marks: Vec<ChordMark>,
    pub harmony: ChordTrack,
    pub warnings: Vec<String>,
}

impl<'a> Ctx<'a> {
    fn new(p: &'a Project) -> Result<Ctx<'a>> {
        let meter = Meter::of(p)?;
        let (key, key_problem) = match p.key.trim() {
            "" => (None, None),
            k => match Scale::parse(k) {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(format!("{e}; chord scales come from the chords alone"))),
            },
        };
        Ok(Ctx {
            p,
            meter,
            bar_beats: meter.bar_beats(),
            key,
            key_problem,
            marks: p.harmony.clone(),
            harmony: ChordTrack::from_project(p),
            warnings: Vec::new(),
        })
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    /// Reports an unreadable project key once (for actions whose result depends on the key).
    pub fn uses_key(&mut self) {
        if let Some(m) = self.key_problem.take() {
            self.warnings.push(m);
        }
    }

    /// The track, or an error listing the tracks there are.
    pub fn track(&self, r: &TrackRef) -> Result<&'a Track> {
        let p: &'a Project = self.p;
        match p.track_index(r) {
            Some(i) => Ok(&p.tracks[i]),
            None => {
                let names: Vec<String> = p.tracks.iter().map(|t| format!("#{} {:?}", t.id, t.name)).collect();
                let list = if names.is_empty() { "the project has no tracks".to_string() } else { format!("the tracks are {}", names.join(", ")) };
                Err(Error::invalid(format!("no track {r} ({list})")))
            }
        }
    }

    /// Checks a bar range against the song; returns its start and end beats.
    pub fn bars(&self, from: u32, to: u32) -> Result<(f64, f64)> {
        if from < 1 {
            return Err(Error::invalid("bars are 1-based: from_bar must be 1 or more"));
        }
        if from > to {
            return Err(Error::invalid(format!("from_bar {from} is after to_bar {to}")));
        }
        if to > self.p.bars {
            return Err(Error::invalid(format!(
                "bar {to} is past the end of the song (bars 1-{}); lengthen it first (set_project bars)",
                self.p.bars
            )));
        }
        Ok((self.bar_start(from), self.bar_start(to + 1)))
    }

    /// An optional bar range, defaulting to the whole song.
    pub fn bars_or_song(&self, from: Option<u32>, to: Option<u32>) -> Result<(u32, u32, f64, f64)> {
        let (from, to) = (from.unwrap_or(1), to.unwrap_or(self.p.bars));
        let (a, b) = self.bars(from, to)?;
        Ok((from, to, a, b))
    }

    pub fn bar_start(&self, bar: u32) -> f64 {
        (bar.max(1) - 1) as f64 * self.bar_beats
    }

    /// The 1-based bar and the beat inside it of an absolute beat.
    pub fn bar_beat(&self, beat: f64) -> (u32, f64) {
        let bar = (beat / self.bar_beats + 1e-9).floor().max(0.0);
        (bar as u32 + 1, beat4((beat - bar * self.bar_beats).max(0.0)))
    }

    /// "bar 5" or "bar 5 beat 2.5".
    pub fn pos(&self, beat: f64) -> String {
        let (bar, b) = self.bar_beat(beat);
        if b.abs() < 1e-6 { format!("bar {bar}") } else { format!("bar {bar} beat {}", fmt_num(b)) }
    }

    /// A pitch name in the key's spelling (flats in flat keys).
    pub fn name(&self, pitch: u8) -> String {
        note_name_with(pitch, self.key.is_some_and(|k| k.prefers_flats()))
    }

    /// `{"bar": 5, "beat": 2.5}` for summaries.
    pub fn at_json(&self, beat: f64) -> Value {
        let (bar, b) = self.bar_beat(beat);
        json!({"bar": bar, "beat": b})
    }

    /// Replaces the chord track from here on (and for SetHarmony).
    pub fn set_marks(&mut self, marks: Vec<ChordMark>) {
        self.harmony = ChordTrack::from_marks(&marks, self.bar_beats, self.p.total_beats());
        self.marks = marks;
    }
}

/// Whether a note starts in `[a, b)` with flux-core's tolerance (the test `RemoveNotes` uses).
pub(crate) fn starts_in(n: &Note, a: f64, b: f64) -> bool {
    n.start >= a - 1e-9 && n.start < b - 1e-9
}

pub(crate) fn is_blip(n: &Note) -> bool {
    n.dur < BLIP_BEATS
}

pub(crate) fn note_spec(n: &Note) -> NoteSpec {
    NoteSpec { bar: None, beat: n.start, dur: n.dur, pitch: PitchSpec::Num(n.pitch), vel: n.vel }
}

pub(crate) fn note_key(n: &Note) -> NoteKey {
    NoteKey { bar: None, beat: n.start, pitch: PitchSpec::Num(n.pitch) }
}

/// Commands that delete the notes of `track` starting in bars `[from, to]`, keeping keyswitch blips, and
/// how many notes they delete.
pub(crate) fn clear_notes(t: &Track, from: u32, to: u32, a: f64, b: f64) -> (Vec<Command>, usize) {
    let in_range: Vec<&Note> = t.notes.iter().filter(|n| starts_in(n, a, b)).collect();
    let doomed: Vec<&Note> = in_range.iter().copied().filter(|n| !is_blip(n)).collect();
    let tref = TrackRef::Id(t.id);
    let cmd = if doomed.is_empty() {
        return (vec![], 0);
    } else if doomed.len() == in_range.len() {
        Command::RemoveNotes { track: tref, from_bar: Some(from), to_bar: Some(to), pitch: None }
    } else {
        Command::DeleteNotes { track: tref, notes: doomed.iter().map(|n| note_key(n)).collect() }
    };
    (vec![cmd], doomed.len())
}

/// `AddNotes` for `notes` (none when empty).
pub(crate) fn add_notes(t: &Track, notes: &[Note]) -> Option<Command> {
    (!notes.is_empty()).then(|| Command::AddNotes { track: TrackRef::Id(t.id), notes: notes.iter().map(note_spec).collect() })
}

/// "Pad bars 5-8" / "Pad bar 5".
pub(crate) fn range_label(t: &Track, from: u32, to: u32) -> String {
    if from == to { format!("{} bar {from}", t.name) } else { format!("{} bars {from}-{to}", t.name) }
}

#[cfg(test)]
mod tests;
