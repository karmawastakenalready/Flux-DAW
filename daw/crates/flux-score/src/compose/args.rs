//! The arguments of each compose action, as typed structs. Unknown fields are rejected so a misspelt
//! option is an error, not a silent default.

use crate::error::{Error, Result};
use crate::phrase::PhraseOptions;
use crate::shapes::Shape;
use crate::theory::note::parse_note;
use flux_core::TrackRef;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A pitch as a MIDI number or a note name ("C4").
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PitchArg(Value);

impl<'de> Deserialize<'de> for PitchArg {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<PitchArg, D::Error> {
        match Value::deserialize(d)? {
            v @ (Value::Number(_) | Value::String(_)) => Ok(PitchArg(v)),
            other => Err(serde::de::Error::custom(format!("expected a MIDI number or a note name like \"C4\", got {other}"))),
        }
    }
}

impl PitchArg {
    pub(crate) fn resolve(&self, what: &str) -> Result<u8> {
        match &self.0 {
            Value::Number(n) => match n.as_f64() {
                Some(x) if x.fract() == 0.0 && (0.0..=127.0).contains(&x) => Ok(x as u8),
                _ => Err(Error::invalid(format!("{what}: MIDI pitch {n} is not a whole number 0-127"))),
            },
            Value::String(s) => parse_note(s).map_err(|e| Error::invalid(format!("{what}: {e}"))),
            other => Err(Error::invalid(format!("{what}: expected a MIDI number or a note name like \"C4\", got {other}"))),
        }
    }
}

/// `start`: one pitch or a list of pitches.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub(crate) enum Pitches {
    Many(Vec<PitchArg>),
    One(PitchArg),
}

impl Pitches {
    pub(crate) fn resolve(&self, what: &str) -> Result<Vec<u8>> {
        match self {
            Pitches::Many(ps) => ps.iter().map(|p| p.resolve(what)).collect(),
            Pitches::One(p) => Ok(vec![p.resolve(what)?]),
        }
    }
}

fn d_true() -> bool {
    true
}
fn d_vel90() -> f64 {
    90.0
}
fn d_vel80() -> f64 {
    80.0
}
fn d_one() -> f64 {
    1.0
}
fn d_short() -> f64 {
    0.85
}
fn d_voices() -> usize {
    4
}
fn d_accent() -> f64 {
    6.0
}
fn d_half() -> f64 {
    0.5
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MelodyArgs {
    pub track: TrackRef,
    pub bar: u32,
    pub notation: String,
    #[serde(default)]
    pub unit: Option<f64>,
    #[serde(default = "d_vel90")]
    pub vel: f64,
    #[serde(default)]
    pub transpose: i32,
    #[serde(default = "d_one")]
    pub legato: f64,
    #[serde(default = "d_short")]
    pub short: f64,
    #[serde(default)]
    pub arc: f64,
    #[serde(default = "d_true")]
    pub replace: bool,
    /// true, or an object of phrase options.
    #[serde(default)]
    pub phrase: Option<Value>,
}

/// Phrase options as JSON (the `phrase` object of melody; also the fields of the phrase action).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct PhraseOpts {
    pub accent: f64,
    pub arc: f64,
    pub soften: f64,
    pub rit: f64,
    pub breath: f64,
    pub vary: f64,
    pub seed: u64,
    pub rest: f64,
    pub long: Option<f64>,
    pub breaks: Vec<u32>,
}

impl Default for PhraseOpts {
    fn default() -> PhraseOpts {
        let d = PhraseOptions::default();
        PhraseOpts { accent: d.accent, arc: d.arc, soften: d.soften, rit: d.rit, breath: d.breath, vary: d.vary, seed: d.seed, rest: d.rest, long: d.long, breaks: d.breaks }
    }
}

impl PhraseOpts {
    pub(crate) fn options(&self) -> Result<PhraseOptions> {
        for (name, v) in [("rit", self.rit), ("breath", self.breath), ("vary", self.vary), ("rest", self.rest)] {
            if !(v.is_finite() && v >= 0.0) {
                return Err(Error::invalid(format!("phrase {name} must be a number >= 0, got {v}")));
            }
        }
        if self.long.is_some_and(|l| !(l.is_finite() && l > 0.0)) {
            return Err(Error::invalid("phrase long must be a positive number of beats"));
        }
        Ok(PhraseOptions {
            accent: self.accent,
            arc: self.arc,
            soften: self.soften,
            rit: self.rit,
            breath: self.breath,
            vary: self.vary,
            seed: self.seed,
            rest: self.rest,
            long: self.long,
            breaks: self.breaks.clone(),
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhraseArgs {
    pub track: TrackRef,
    pub from_bar: u32,
    pub to_bar: u32,
    /// Base velocity: every note starts from it (so re-running gives the same velocities).
    #[serde(default)]
    pub vel: Option<f64>,
    #[serde(default = "d_accent")]
    pub accent: f64,
    #[serde(default = "d_eight")]
    pub arc: f64,
    #[serde(default = "d_eight")]
    pub soften: f64,
    #[serde(default)]
    pub rit: f64,
    #[serde(default)]
    pub breath: f64,
    #[serde(default = "d_two")]
    pub vary: f64,
    #[serde(default)]
    pub seed: u64,
    #[serde(default = "d_half")]
    pub rest: f64,
    #[serde(default)]
    pub long: Option<f64>,
    #[serde(default)]
    pub breaks: Vec<u32>,
}

fn d_eight() -> f64 {
    8.0
}
fn d_two() -> f64 {
    2.0
}

impl PhraseArgs {
    pub(crate) fn opts(&self) -> PhraseOpts {
        PhraseOpts {
            accent: self.accent,
            arc: self.arc,
            soften: self.soften,
            rit: self.rit,
            breath: self.breath,
            vary: self.vary,
            seed: self.seed,
            rest: self.rest,
            long: self.long,
            breaks: self.breaks.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChordsArgs {
    pub track: TrackRef,
    pub from_bar: u32,
    pub to_bar: u32,
    #[serde(default)]
    pub chords: Option<String>,
    #[serde(default = "d_true")]
    pub set_harmony: bool,
    #[serde(default = "d_voices")]
    pub voices: usize,
    #[serde(default)]
    pub low: Option<PitchArg>,
    #[serde(default)]
    pub high: Option<PitchArg>,
    #[serde(default = "d_vel80")]
    pub vel: f64,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default = "d_one")]
    pub gate: f64,
    #[serde(default = "d_accent")]
    pub accent: f64,
    #[serde(default)]
    pub bass_track: Option<TrackRef>,
    #[serde(default)]
    pub start: Option<Pitches>,
    #[serde(default = "d_true")]
    pub replace: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BassArgs {
    pub track: TrackRef,
    pub from_bar: u32,
    pub to_bar: u32,
    #[serde(default)]
    pub chords: Option<String>,
    #[serde(default = "d_true")]
    pub set_harmony: bool,
    #[serde(default)]
    pub low: Option<PitchArg>,
    #[serde(default)]
    pub high: Option<PitchArg>,
    #[serde(default = "d_true")]
    pub inversions: bool,
    #[serde(default = "d_vel90")]
    pub vel: f64,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default = "d_one")]
    pub gate: f64,
    #[serde(default = "d_accent")]
    pub accent: f64,
    #[serde(default)]
    pub start: Option<Pitches>,
    #[serde(default = "d_true")]
    pub replace: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArpeggioArgs {
    pub track: TrackRef,
    pub from_bar: u32,
    pub to_bar: u32,
    #[serde(default)]
    pub chords: Option<String>,
    #[serde(default = "d_true")]
    pub set_harmony: bool,
    #[serde(default = "d_voices")]
    pub voices: usize,
    #[serde(default)]
    pub low: Option<PitchArg>,
    #[serde(default)]
    pub high: Option<PitchArg>,
    #[serde(default = "d_vel80")]
    pub vel: f64,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default = "d_one")]
    pub gate: f64,
    #[serde(default = "d_accent")]
    pub accent: f64,
    #[serde(default)]
    pub start: Option<Pitches>,
    #[serde(default = "d_true")]
    pub replace: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SwellArgs {
    pub track: TrackRef,
    pub from_bar: u32,
    pub to_bar: u32,
    #[serde(default = "d_cc1")]
    pub cc: u8,
    #[serde(default = "d_lo")]
    pub lo: f64,
    #[serde(default = "d_hi")]
    pub hi: f64,
    #[serde(default = "d_long")]
    pub long: f64,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default = "d_one")]
    pub rest: f64,
    #[serde(default)]
    pub source: Option<TrackRef>,
}

fn d_cc1() -> u8 {
    1
}
fn d_lo() -> f64 {
    40.0
}
fn d_hi() -> f64 {
    100.0
}
fn d_long() -> f64 {
    1.5
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CurvePointArg {
    #[serde(default)]
    pub bar: Option<u32>,
    #[serde(default)]
    pub beat: f64,
    pub value: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CurveArgs {
    pub track: TrackRef,
    pub points: Vec<CurvePointArg>,
    #[serde(default)]
    pub cc: Option<u8>,
    #[serde(default)]
    pub bend: bool,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub shape: Shape,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarmonyArgs {
    #[serde(default)]
    pub from_bar: Option<u32>,
    #[serde(default)]
    pub to_bar: Option<u32>,
    #[serde(default)]
    pub chords: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Fix {
    #[default]
    None,
    Strong,
    All,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FitArgs {
    pub track: TrackRef,
    #[serde(default)]
    pub from_bar: Option<u32>,
    #[serde(default)]
    pub to_bar: Option<u32>,
    #[serde(default)]
    pub fix: Fix,
    /// Semitones the track sounds away from its written notes (the instrument's transposition).
    #[serde(default)]
    pub transpose: i32,
}
