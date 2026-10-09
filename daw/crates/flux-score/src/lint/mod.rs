//! The score linter: finds what a listener would hear as a mistake, from the project alone.
//!
//! [`lint`] ports `tools/score_lint.py` (clashes, strong-beat non-chord tones, ranges, low-register mud)
//! with the chord track (`Project::harmony`) as its harmony, and adds part-writing and data-hygiene checks:
//!
//! | check         | finds                                                                                     |
//! |---------------|-------------------------------------------------------------------------------------------|
//! | `clash`       | two tracks a minor 2nd / major 7th / minor 9th apart for half a beat or more (passing notes, and a 2nd or 7th inside the chord track's chord, excepted) |
//! | `nct`         | melody notes on strong beats that are not chord tones: appoggiaturas, or clashes           |
//! | `range`       | notes on keys the instrument does not play (from its catalog card or the caller)           |
//! | `mud`         | more than 3 sustained pitches within an octave below C3                                   |
//! | `parallels`   | parallel fifths and octaves between the melody and the bass                                |
//! | `crossing`    | a lower line moving above a higher one                                                     |
//! | `leaps`       | leaps wider than `max_leap` inside a phrase                                               |
//! | `overlaps`    | the same pitch twice at once on one track (duplicates, overlapping repeats)                |
//! | `controllers` | several CC or pitch-bend points of one kind at the same beat (pile-ups from appending)     |
//! | `past_end`    | notes and controller data after the end of the song                                       |
//! | `keyswitches` | keyswitch keys held as notes, stray blips, keyswitches at the same instant as their notes  |
//! | `harmony`     | chord marks that cannot be read                                                           |
//!
//! Pitches are judged as heard: the caller's per-track transposition and the pitch bend (at `bend_range`
//! semitones) are applied, except by `range`, `overlaps` and `keyswitches`, which look at the keys as
//! written. Notes under [`MIN_DUR`] beats are keyswitch blips and only `keyswitches` looks at them.
//! Repeated findings are merged: a finding lists the bar where it first happens and `also_bars`.

mod catalog;
mod checks;
mod extra;
mod handler;
mod prep;

pub use catalog::{InstrumentCatalog, InstrumentInfo, normalize_path};
pub use handler::{lint_handler, lint_handler_with, lint_schema, lint_tool};

use crate::harmony::ChordTrack;
use crate::meter::Meter;
use crate::theory::note::SHARP_NAMES;
use crate::util::{OrderedMap, beat4};
use flux_core::{Instrument, Project, TrackRef};
use prep::{LTrack, heard_notes, top_median, unpitched_name};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Notes shorter than this (in beats) are keyswitch blips.
pub const MIN_DUR: f64 = 0.05;
/// Beats of timing slop when matching grid positions and onsets.
pub const TOL: f64 = 0.03;

/// The checks [`lint`] can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Clash,
    Nct,
    Range,
    Mud,
    Parallels,
    Crossing,
    Leaps,
    Overlaps,
    Controllers,
    PastEnd,
    Keyswitches,
    Harmony,
}

impl Check {
    pub const ALL: [Check; 12] = [
        Check::Clash,
        Check::Nct,
        Check::Range,
        Check::Mud,
        Check::Parallels,
        Check::Crossing,
        Check::Leaps,
        Check::Overlaps,
        Check::Controllers,
        Check::PastEnd,
        Check::Keyswitches,
        Check::Harmony,
    ];
    /// The checks tools/score_lint.py has.
    pub const PYTHON: [Check; 4] = [Check::Clash, Check::Nct, Check::Range, Check::Mud];

    pub fn name(self) -> &'static str {
        match self {
            Check::Clash => "clash",
            Check::Nct => "nct",
            Check::Range => "range",
            Check::Mud => "mud",
            Check::Parallels => "parallels",
            Check::Crossing => "crossing",
            Check::Leaps => "leaps",
            Check::Overlaps => "overlaps",
            Check::Controllers => "controllers",
            Check::PastEnd => "past_end",
            Check::Keyswitches => "keyswitches",
            Check::Harmony => "harmony",
        }
    }

    pub fn parse(s: &str) -> Option<Check> {
        Check::ALL.into_iter().find(|c| c.name() == s.trim())
    }
}

/// What a finding is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Two tracks a minor 2nd, major 7th or minor 9th apart, sustained.
    Clash,
    /// A strong-beat non-chord tone that resolves by step to a chord tone.
    NctAppoggiatura,
    /// A strong-beat non-chord tone that does not resolve.
    NctClash,
    /// A key the instrument does not play.
    Range,
    /// Too many sustained low pitches within an octave.
    Mud,
    ParallelFifths,
    ParallelOctaves,
    VoiceCrossing,
    Leap,
    /// The same pitch sounding twice at once on one track.
    SelfOverlap,
    /// Several controller points of one kind at the same beat.
    ControllerPileUp,
    /// Notes or controller data after the end of the song.
    PastEnd,
    Keyswitch,
    /// A chord mark that cannot be read.
    ChordSymbol,
}

impl Kind {
    pub fn check(self) -> Check {
        match self {
            Kind::Clash => Check::Clash,
            Kind::NctAppoggiatura | Kind::NctClash => Check::Nct,
            Kind::Range => Check::Range,
            Kind::Mud => Check::Mud,
            Kind::ParallelFifths | Kind::ParallelOctaves => Check::Parallels,
            Kind::VoiceCrossing => Check::Crossing,
            Kind::Leap => Check::Leaps,
            Kind::SelfOverlap => Check::Overlaps,
            Kind::ControllerPileUp => Check::Controllers,
            Kind::PastEnd => Check::PastEnd,
            Kind::Keyswitch => Check::Keyswitches,
            Kind::ChordSymbol => Check::Harmony,
        }
    }

    /// The kind's name in JSON ("nct_appoggiatura").
    pub fn name(self) -> &'static str {
        match self {
            Kind::Clash => "clash",
            Kind::NctAppoggiatura => "nct_appoggiatura",
            Kind::NctClash => "nct_clash",
            Kind::Range => "range",
            Kind::Mud => "mud",
            Kind::ParallelFifths => "parallel_fifths",
            Kind::ParallelOctaves => "parallel_octaves",
            Kind::VoiceCrossing => "voice_crossing",
            Kind::Leap => "leap",
            Kind::SelfOverlap => "self_overlap",
            Kind::ControllerPileUp => "controller_pile_up",
            Kind::PastEnd => "past_end",
            Kind::Keyswitch => "keyswitch",
            Kind::ChordSymbol => "chord_symbol",
        }
    }

    /// score_lint.py's kind names (findings in a bar are ordered by them, then by message).
    fn sort_name(self) -> &'static str {
        match self {
            Kind::Clash => "clash",
            Kind::NctAppoggiatura => "nct-app",
            Kind::NctClash => "nct-clash",
            Kind::Range => "range",
            Kind::Mud => "mud",
            Kind::ParallelFifths => "parallel-fifths",
            Kind::ParallelOctaves => "parallel-octaves",
            Kind::VoiceCrossing => "voice-crossing",
            Kind::Leap => "leap",
            Kind::SelfOverlap => "self-overlap",
            Kind::ControllerPileUp => "controller-pileup",
            Kind::PastEnd => "past-end",
            Kind::Keyswitch => "keyswitch",
            Kind::ChordSymbol => "chord-symbol",
        }
    }

    /// The label in the text report (at most 5 characters, like score_lint.py's).
    fn label(self) -> &'static str {
        match self {
            Kind::Clash => "clash",
            Kind::NctAppoggiatura | Kind::NctClash => "nct",
            Kind::Range => "range",
            Kind::Mud => "mud",
            Kind::ParallelFifths | Kind::ParallelOctaves => "paral",
            Kind::VoiceCrossing => "cross",
            Kind::Leap => "leap",
            Kind::SelfOverlap => "same",
            Kind::ControllerPileUp => "ctrl",
            Kind::PastEnd => "end",
            Kind::Keyswitch => "keysw",
            Kind::ChordSymbol => "chord",
        }
    }

    /// What `n` findings of this kind are, for the summary line.
    fn counted(self, n: usize) -> String {
        let (one, many) = match self {
            Kind::Clash => ("sustained clash", "sustained clashes"),
            Kind::NctAppoggiatura => ("appoggiatura", "appoggiaturas"),
            Kind::NctClash => ("strong-beat clash", "strong-beat clashes"),
            Kind::Range => ("out-of-range note", "out-of-range notes"),
            Kind::Mud => ("muddy spot", "muddy spots"),
            Kind::ParallelFifths => ("parallel fifths", "parallel fifths"),
            Kind::ParallelOctaves => ("parallel octaves", "parallel octaves"),
            Kind::VoiceCrossing => ("voice crossing", "voice crossings"),
            Kind::Leap => ("wide leap", "wide leaps"),
            Kind::SelfOverlap => ("same-pitch overlap", "same-pitch overlaps"),
            Kind::ControllerPileUp => ("controller pile-up", "controller pile-ups"),
            Kind::PastEnd => ("past the end", "past the end"),
            Kind::Keyswitch => ("keyswitch problem", "keyswitch problems"),
            Kind::ChordSymbol => ("chord mark problem", "chord mark problems"),
        };
        format!("{n} {}", if n == 1 { one } else { many })
    }
}

/// How much a finding matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing; often intended (appoggiaturas, parallels, crossings, leaps).
    Info,
    /// Probably heard as a mistake.
    Warning,
    /// Wrong data: unplayable or unheard notes, duplicates, unreadable chords.
    Error,
}

/// One finding (merged over the bars where the same thing happens).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub kind: Kind,
    pub severity: Severity,
    /// Where it first happens: 1-based bar and the beat inside it.
    pub bar: u32,
    pub beat: f64,
    pub tracks: Vec<String>,
    pub message: String,
    /// A suggested fix.
    pub fix: String,
    /// Other bars where the same finding happens.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_bars: Vec<u32>,
}

impl Finding {
    /// The number of bars it happens in.
    pub fn occurrences(&self) -> usize {
        1 + self.also_bars.len()
    }
}

/// What the caller tells the linter.
#[derive(Clone, Debug)]
pub struct LintOptions {
    /// Instrument cards for the range and keyswitch checks (None: no cards).
    pub catalog: Option<InstrumentCatalog>,
    /// Playable keys of tracks, overriding their cards: (track, lowest, highest).
    pub ranges: Vec<(TrackRef, u8, u8)>,
    /// Sounding pitch minus written key for tracks whose instrument is mapped off pitch.
    pub transpose: Vec<(TrackRef, i32)>,
    /// The melody tracks (None: guessed, as tracks whose top line lies mostly at C4 or above).
    pub melody: Option<Vec<TrackRef>>,
    /// The checks to run (None: all).
    pub checks: Option<Vec<Check>>,
    /// Semitones at full pitch bend.
    pub bend_range: f64,
    /// Only report findings in these bars.
    pub from_bar: Option<u32>,
    pub to_bar: Option<u32>,
    /// The widest leap (semitones) inside a phrase that is not reported.
    pub max_leap: u8,
}

impl Default for LintOptions {
    fn default() -> LintOptions {
        LintOptions { catalog: None, ranges: vec![], transpose: vec![], melody: None, checks: None, bend_range: 2.0, from_bar: None, to_bar: None, max_leap: 12 }
    }
}

/// Which instrument ranges the range check could use.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RangeCoverage {
    /// Cards in the catalog.
    pub cards: usize,
    /// Pitched tracks with a known range.
    pub matched: Vec<String>,
    /// Pitched tracks without one.
    pub unmatched: Vec<String>,
}

/// The result of [`lint`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LintReport {
    pub project: String,
    pub meter: String,
    pub bars: u32,
    /// Pitched tracks with notes.
    pub pitched_tracks: Vec<String>,
    pub melody: Vec<String>,
    pub melody_guessed: bool,
    /// Marks on the chord track.
    pub chord_marks: usize,
    pub ranges: RangeCoverage,
    /// Tracks judged at a transposed pitch.
    pub transposed: Vec<(String, i32)>,
    /// Tracks whose pitch bends were applied.
    pub bent: Vec<String>,
    pub bend_range: f64,
    pub checks: Vec<Check>,
    /// Sorted by first bar, then kind, then message.
    pub findings: Vec<Finding>,
    /// Remarks about the run itself (unknown tracks in the options, checks that could not run).
    pub notes: Vec<String>,
}

impl LintReport {
    /// How often findings of `kind` happen, counted per bar (score_lint.py's counts).
    pub fn count(&self, kind: Kind) -> usize {
        self.findings.iter().filter(|f| f.kind == kind).map(Finding::occurrences).sum()
    }

    /// Per-bar counts by kind (kinds that were found only).
    pub fn counts(&self) -> BTreeMap<Kind, usize> {
        let mut m = BTreeMap::new();
        for f in &self.findings {
            *m.entry(f.kind).or_insert(0) += f.occurrences();
        }
        m
    }

    /// The report as text, in score_lint.py's layout (the first `max` findings).
    pub fn to_text(&self, max: usize) -> String {
        let mut s = String::new();
        let has = |c: Check| self.checks.contains(&c);
        let _ = writeln!(s, "{}: {}, {} bars, {} pitched tracks", self.project, self.meter, self.bars, self.pitched_tracks.len());
        let _ = writeln!(s, "  melody: {}", if self.melody.is_empty() { "-".to_string() } else { self.melody.join(", ") });
        let _ = writeln!(s, "  harmony: {}", if self.chord_marks == 0 { "the chord track is empty".to_string() } else { format!("chord track ({} marks)", self.chord_marks) });
        let _ = writeln!(s, "  range: {} cards, {} of {} pitched tracks matched", self.ranges.cards, self.ranges.matched.len(), self.pitched_tracks.len());
        if !self.transposed.is_empty() {
            let t: Vec<String> = self.transposed.iter().map(|(n, k)| format!("{n} {k:+}")).collect();
            let _ = writeln!(s, "  transposed to sounding pitch: {}", t.join(", "));
        }
        if !self.bent.is_empty() {
            let _ = writeln!(s, "  pitch bends applied (+-{} semitones) on: {}", crate::util::fmt_num(self.bend_range), self.bent.join(", "));
        }
        let (napp, ncl) = (self.count(Kind::NctAppoggiatura), self.count(Kind::NctClash));
        let _ = writeln!(
            s,
            "  found: {} sustained clashes, {}{} out-of-range notes, {} muddy spots (counted per bar)",
            self.count(Kind::Clash),
            if self.chord_marks > 0 && has(Check::Nct) { format!("{} strong-beat non-chord tones ({napp} appoggiaturas, {ncl} clashes), ", napp + ncl) } else { String::new() },
            self.count(Kind::Range),
            self.count(Kind::Mud)
        );
        let extra: Vec<String> = self
            .counts()
            .into_iter()
            .filter(|(k, _)| !Check::PYTHON.contains(&k.check()))
            .map(|(k, n)| k.counted(n))
            .collect();
        if !extra.is_empty() {
            let _ = writeln!(s, "  also: {}", extra.join(", "));
        }
        for n in &self.notes {
            let _ = writeln!(s, "  note: {n}");
        }
        let mut last = None;
        for f in self.findings.iter().take(max) {
            let more = if f.also_bars.is_empty() { String::new() } else { format!("   (also bars {})", bar_spans(&f.also_bars)) };
            let at = if last == Some(f.bar) { String::new() } else { format!("bar {}", f.bar) };
            let _ = writeln!(s, "{at:>8}  {:<6}{}{more}", f.kind.label(), f.message);
            last = Some(f.bar);
        }
        if self.findings.len() > max {
            let _ = writeln!(s, "  ... {} more (max)", self.findings.len() - max);
        }
        s
    }

    /// The report as JSON for an agent: the header, per-kind counts and the first `max` findings.
    pub fn to_json(&self, max: usize) -> Value {
        let mut v = json!({
            "project": self.project,
            "meter": self.meter,
            "bars": self.bars,
            "pitched_tracks": self.pitched_tracks,
            "melody": self.melody,
            "melody_guessed": self.melody_guessed,
            "chord_marks": self.chord_marks,
            "ranges": self.ranges,
            "checks": self.checks,
            "counts": self.counts().into_iter().map(|(k, n)| (k.name().to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
            "findings": self.findings.iter().take(max).collect::<Vec<_>>(),
        });
        if !self.transposed.is_empty() {
            v["transposed"] = json!(self.transposed.iter().map(|(n, k)| (n.clone(), json!(k))).collect::<serde_json::Map<_, _>>());
        }
        if !self.bent.is_empty() {
            v["bends_applied"] = json!({"tracks": self.bent, "semitones": self.bend_range});
        }
        if self.findings.len() > max {
            v["more"] = json!(self.findings.len() - max);
        }
        if !self.notes.is_empty() {
            v["notes"] = json!(self.notes);
        }
        v
    }
}

/// "3-5, 9" from sorted bars (score_lint.py's `spans`).
fn bar_spans(bars: &[u32]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bars.len() {
        let mut j = i;
        while j + 1 < bars.len() && bars[j + 1] == bars[j] + 1 {
            j += 1;
        }
        out.push(if j > i { format!("{}-{}", bars[i], bars[j]) } else { bars[i].to_string() });
        i = j + 1;
    }
    out.join(", ")
}

/// A pitch name for any heard pitch (score.py's `note_name`, which also names pitches past MIDI 0-127).
pub(crate) fn pname(p: i32) -> String {
    format!("{}{}", SHARP_NAMES[p.rem_euclid(12) as usize], p.div_euclid(12) - 1)
}

/// A finding as a check reports it, before merging.
pub(crate) struct Draft {
    pub kind: Kind,
    pub severity: Severity,
    pub tracks: Vec<String>,
    pub message: String,
    pub fix: String,
}

/// One finding being collected.
struct Item {
    draft: Draft,
    time: f64,
    bars: Vec<u32>,
}

/// Collects findings, merging the same finding in other bars (score_lint.py's `Report`).
pub(crate) struct Acc {
    bar_len: f64,
    range: (u32, u32),
    items: OrderedMap<(Kind, String), Item>,
}

impl Acc {
    fn new(bar_len: f64, range: (u32, u32)) -> Acc {
        Acc { bar_len, range, items: OrderedMap::default() }
    }

    /// The bar of a time, with [`TOL`] of slop (a note a hair early belongs to the next bar).
    pub fn bar(&self, t: f64) -> u32 {
        (((t + TOL) / self.bar_len).floor().max(0.0) as u32) + 1
    }

    pub fn bar_len(&self) -> f64 {
        self.bar_len
    }

    /// Whether findings at time `t` are reported (its bar is inside the requested bars).
    pub fn in_range(&self, t: f64) -> bool {
        (self.range.0..=self.range.1).contains(&self.bar(t))
    }

    /// Adds a finding at time `t`. The same `key` (within its kind) in another bar only adds the bar: the
    /// finding keeps the message of its first bar.
    pub fn add(&mut self, t: f64, key: String, draft: Draft) {
        if !self.in_range(t) {
            return;
        }
        let b = self.bar(t);
        let it = self.items.entry_or_insert_with((draft.kind, key), || Item { draft, time: t, bars: vec![] });
        if !it.bars.contains(&b) {
            it.bars.push(b);
        }
    }

    fn findings(self) -> Vec<Finding> {
        let bar_len = self.bar_len;
        let mut out: Vec<Finding> = self
            .items
            .into_entries()
            .into_iter()
            .map(|(_, it)| {
                let first = it.bars[0];
                let beat = beat4((it.time - (first - 1) as f64 * bar_len).clamp(0.0, bar_len));
                let mut also: Vec<u32> = it.bars[1..].to_vec();
                also.sort_unstable();
                let d = it.draft;
                Finding { kind: d.kind, severity: d.severity, bar: first, beat, tracks: d.tracks, message: d.message, fix: d.fix, also_bars: also }
            })
            .collect();
        out.sort_by(|a, b| (a.bar, a.kind.sort_name(), &a.message).cmp(&(b.bar, b.kind.sort_name(), &b.message)));
        out
    }
}

/// The instrument path of a track, if it has one.
fn instrument_path(i: &Instrument) -> &str {
    match i {
        Instrument::Sfz { path, .. } | Instrument::Vst3 { path, .. } | Instrument::Audio { path } => path,
        Instrument::None => "",
    }
}

/// Lints `project`. Never fails: problems with the options (an unknown track, say) are listed in
/// [`LintReport::notes`].
///
/// ```
/// # use flux_core::{Project, Command, ChordMark, commands::apply};
/// # let mut project: Project = serde_json::from_value(serde_json::json!({
/// #     "name": "demo", "tempo": [{"bar": 1, "bpm": 90.0}], "time_signature": [4, 4], "bars": 4, "tracks": []})).unwrap();
/// # apply(&mut project, &Command::SetHarmony { chords: vec![ChordMark { bar: 1, beat: 0.0, chord: "C".into() }] }).unwrap();
/// # for (name, notation) in [("Flute", "F5:4 | E5:4"), ("Horn", "E4:4 | C4:4")] {
/// #     apply(&mut project, &Command::AddTrack { name: name.into(), instrument: flux_core::Instrument::None, color: None }).unwrap();
/// #     let out = flux_score::compose(&project, &serde_json::json!({"action": "melody", "track": name, "bar": 1, "notation": notation})).unwrap();
/// #     for c in &out.commands { apply(&mut project, c).unwrap(); }
/// # }
/// use flux_score::lint::{lint, Kind, LintOptions};
/// let report = lint(&project, &LintOptions::default());
/// // the flute's F5 rubs a minor 9th against the horn's E4, and is not a chord tone of C on the downbeat
/// assert_eq!(report.count(Kind::Clash), 1);
/// assert_eq!(report.count(Kind::NctAppoggiatura), 1);
/// println!("{}", report.to_text(40));
/// ```
pub fn lint(project: &Project, opts: &LintOptions) -> LintReport {
    let mut notes: Vec<String> = Vec::new();
    let (num, den) = project.time_signature;
    let meter = Meter::new(num, den).unwrap_or_else(|e| {
        notes.push(format!("time signature {num}/{den}: {e}; judging the metre as 4/4"));
        Meter::default()
    });
    let bar_len = meter.bar_beats();
    let checks: Vec<Check> = match &opts.checks {
        Some(c) => {
            let mut c = c.clone();
            c.sort_unstable();
            c.dedup();
            c
        }
        None => Check::ALL.to_vec(),
    };
    let on = |c: Check| checks.contains(&c);
    let resolve = |r: &TrackRef, what: &str, notes: &mut Vec<String>| -> Option<usize> {
        let i = project.track_index(r);
        if i.is_none() {
            notes.push(format!("{what}: no track {r}"));
        }
        i
    };

    // tracks as heard
    let mut shift: BTreeMap<usize, i32> = BTreeMap::new();
    for (r, k) in &opts.transpose {
        if let Some(i) = resolve(r, "transpose", &mut notes) {
            shift.insert(i, *k);
        }
    }
    let mut given_range: BTreeMap<usize, (u8, u8)> = BTreeMap::new();
    for (r, lo, hi) in &opts.ranges {
        if let Some(i) = resolve(r, "ranges", &mut notes) {
            given_range.insert(i, (*lo, *hi));
        }
    }
    let tracks: Vec<LTrack> = project
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let path = instrument_path(&t.instrument);
            let card = if path.is_empty() { None } else { opts.catalog.as_ref().and_then(|c| c.find(path)) };
            let info = match (given_range.get(&i), card) {
                (Some(&(lo, hi)), card) => Some(InstrumentInfo {
                    ranges: vec![(lo.min(hi), lo.max(hi))],
                    source: "given range".into(),
                    ..card.cloned().unwrap_or_else(|| InstrumentInfo::with_range(&t.name, lo, hi))
                }),
                (None, card) => card.cloned(),
            };
            let pitched = !unpitched_name(&format!("{} {path}", t.name)) && info.as_ref().is_none_or(|c| c.pitched != Some(false));
            let k = shift.get(&i).copied().unwrap_or(0);
            let (held_switches, notes): (Vec<_>, Vec<_>) =
                heard_notes(t, i, k, opts.bend_range).into_iter().partition(|n| info.as_ref().is_some_and(|c| c.keyswitches.contains_key(&n.key) && !c.plays(n.key)));
            LTrack {
                track: t,
                pitched,
                notes,
                blips: t.notes.iter().filter(|n| n.dur < MIN_DUR).copied().collect(),
                held_switches,
                bent: !t.bends.is_empty(),
                info,
            }
        })
        .collect();
    let pitched: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].pitched && !tracks[i].notes.is_empty()).collect();

    // the melody
    let (melody, melody_guessed): (Vec<usize>, bool) = match &opts.melody {
        Some(refs) => (refs.iter().filter_map(|r| resolve(r, "melody", &mut notes)).collect(), false),
        None => (pitched.iter().copied().filter(|&i| top_median(&tracks[i].notes).is_some_and(|m| m >= 60)).collect(), true),
    };

    let harmony = ChordTrack::from_project(project);
    let range = (opts.from_bar.unwrap_or(1).max(1), opts.to_bar.unwrap_or(u32::MAX));
    let mut acc = Acc::new(bar_len, range);
    if on(Check::Clash) {
        checks::clashes(&tracks, &harmony, &mut acc);
    }
    if on(Check::Mud) {
        checks::mud(&tracks, &mut acc);
    }
    if on(Check::Nct) {
        if harmony.is_empty() {
            notes.push("nct: skipped, the chord track is empty".into());
        } else {
            checks::ncts(&tracks, &harmony, meter, &melody, &mut acc);
        }
    }
    let mut coverage = RangeCoverage { cards: opts.catalog.as_ref().map_or(0, InstrumentCatalog::len), ..Default::default() };
    for &i in &pitched {
        let name = tracks[i].name().to_string();
        if tracks[i].info.is_some() { coverage.matched.push(name) } else { coverage.unmatched.push(name) }
    }
    if on(Check::Range) {
        checks::range(&tracks, &mut acc);
    }
    if on(Check::Parallels) {
        extra::parallels(&tracks, &melody, &mut acc);
    }
    if on(Check::Crossing) {
        extra::crossing(&tracks, &mut acc);
    }
    if on(Check::Leaps) {
        extra::leaps(&tracks, &melody, opts.max_leap, &mut acc);
    }
    if on(Check::Overlaps) {
        extra::overlaps(&tracks, &mut acc);
    }
    if on(Check::Controllers) {
        extra::controllers(&tracks, &mut acc);
    }
    if on(Check::PastEnd) {
        extra::past_end(&tracks, project, &mut acc);
    }
    if on(Check::Keyswitches) {
        extra::keyswitches(&tracks, &mut acc);
    }
    if on(Check::Harmony) {
        extra::chord_symbols(&harmony, project.total_beats(), &mut acc);
    }

    LintReport {
        project: project.name.clone(),
        meter: format!("{num}/{den}"),
        bars: project.bars,
        pitched_tracks: pitched.iter().map(|&i| tracks[i].name().to_string()).collect(),
        melody: melody.iter().map(|&i| tracks[i].name().to_string()).collect(),
        melody_guessed,
        chord_marks: project.harmony.len(),
        ranges: coverage,
        transposed: shift.iter().map(|(&i, &k)| (tracks[i].name().to_string(), k)).collect(),
        bent: pitched.iter().filter(|&&i| tracks[i].bent).map(|&i| tracks[i].name().to_string()).collect(),
        bend_range: opts.bend_range,
        checks,
        findings: acc.findings(),
        notes,
    }
}

#[cfg(test)]
mod tests;
