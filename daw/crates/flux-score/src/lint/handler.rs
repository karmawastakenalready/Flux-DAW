//! The `lint` MCP tool: JSON arguments in, the report out.

use super::{Check, InstrumentCatalog, LintOptions, Severity, lint};
use crate::compose::args::PitchArg;
use crate::error::{Error, Result};
use flux_core::{Project, TrackRef};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const DESCRIPTION: &str = "Lint the score for what a listener would hear as a mistake, from the project alone (no render). \
Checks: clash (two tracks a minor 2nd / major 7th / minor 9th apart for half a beat or more; a 2nd or 7th between two tones of the chord track's chord, as in a maj7 or m9, is not one), nct (melody notes on strong beats \
that are not tones of the chord track's chord: appoggiaturas or clashes; needs the chord track), range (keys the instrument \
does not play, from catalog cards or `ranges`), mud (more than 3 sustained pitches within an octave below C3), parallels \
(fifths/octaves between melody and bass), crossing, leaps, overlaps (duplicate or overlapping notes of one pitch on a track), \
controllers (several CC or bend points at one beat), past_end, keyswitches, harmony (unreadable chord marks). \
Findings are merged across bars (`also_bars`) and sorted by bar; each has a severity and a suggested fix. \
Pitch bends (+-bend_range semitones) and `transpose` are applied; keyswitch blips (notes under 0.05 beat) are ignored.";

/// The JSON Schema of the `lint` tool's arguments.
pub fn lint_schema() -> Value {
    let names: Vec<&str> = Check::ALL.iter().map(|c| c.name()).collect();
    json!({
        "type": "object",
        "properties": {
            "from_bar": {"type": "integer", "minimum": 1, "description": "Only report findings from this bar (1-based)."},
            "to_bar": {"type": "integer", "minimum": 1, "description": "Only report findings up to this bar."},
            "checks": {"type": "array", "items": {"type": "string", "enum": names}, "description": "Checks to run (default: all)."},
            "melody": {
                "type": "array",
                "items": {"type": ["string", "integer"]},
                "description": "Melody tracks by name or id (default: guessed, the tracks whose top line lies mostly at C4 or above)."
            },
            "transpose": {
                "type": "object",
                "additionalProperties": {"type": "integer", "minimum": -48, "maximum": 48},
                "description": "Track name -> semitones the instrument sounds above its written keys (e.g. {\"Bass\": -12}). Used for every check but range."
            },
            "ranges": {
                "type": "object",
                "additionalProperties": {"type": "array", "items": {"type": ["integer", "string"]}, "minItems": 2, "maxItems": 2},
                "description": "Track name -> [lowest, highest] playable key (MIDI number or name like \"E2\"), overriding the instrument card."
            },
            "catalog_dir": {"type": "string", "description": "Folder of instrument cards (catalog/*.json) for the range and keyswitch checks."},
            "bend_range": {"type": "number", "exclusiveMinimum": 0, "maximum": 96, "description": "Semitones at full pitch bend (default 2)."},
            "max_leap": {"type": "integer", "minimum": 1, "maximum": 127, "description": "Widest leap inside a phrase that is not reported (default 12)."},
            "min_severity": {"type": "string", "enum": ["info", "warning", "error"], "description": "Leave out findings below this severity (default info)."},
            "max_findings": {"type": "integer", "minimum": 0, "maximum": 10000, "description": "Findings to list (default 40; the counts cover all)."},
            "format": {"type": "string", "enum": ["json", "text"], "description": "json (default): structured findings; text: the report as lines."}
        },
        "additionalProperties": false
    })
}

/// The `lint` tool's definition for an MCP `tools/list` reply.
pub fn lint_tool() -> Value {
    json!({"name": "lint", "description": DESCRIPTION, "inputSchema": lint_schema()})
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    #[default]
    Json,
    Text,
}

fn default_max() -> usize {
    40
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LintArgs {
    #[serde(default)]
    from_bar: Option<u32>,
    #[serde(default)]
    to_bar: Option<u32>,
    #[serde(default)]
    checks: Option<Vec<String>>,
    #[serde(default)]
    melody: Option<Vec<TrackRef>>,
    #[serde(default)]
    transpose: BTreeMap<String, i32>,
    #[serde(default)]
    ranges: BTreeMap<String, (PitchArg, PitchArg)>,
    #[serde(default)]
    catalog_dir: Option<String>,
    #[serde(default)]
    bend_range: Option<f64>,
    #[serde(default)]
    max_leap: Option<u8>,
    #[serde(default)]
    min_severity: Option<Severity>,
    #[serde(default = "default_max")]
    max_findings: usize,
    #[serde(default)]
    format: Format,
}

/// Runs the `lint` tool. A `catalog_dir` argument loads instrument cards from that folder; without one the
/// range and keyswitch checks only know the `ranges` given.
///
/// ```
/// # let project: flux_core::Project = serde_json::from_value(serde_json::json!({
/// #     "name": "demo", "tempo": [{"bar": 1, "bpm": 90.0}], "time_signature": [4, 4], "bars": 4, "tracks": []})).unwrap();
/// let out = flux_score::lint::lint_handler(&project, &serde_json::json!({"checks": ["clash", "mud"], "max_findings": 10})).unwrap();
/// assert_eq!(out["findings"], serde_json::json!([]));
/// ```
pub fn lint_handler(project: &Project, args: &Value) -> Result<Value> {
    lint_handler_with(project, args, &LintOptions::default())
}

/// [`lint_handler`] on top of `base` options (a server that keeps its instrument catalog loaded passes it
/// here); the arguments override them.
pub fn lint_handler_with(project: &Project, args: &Value, base: &LintOptions) -> Result<Value> {
    let bad = |m: String| Error::invalid(format!("lint: {m}"));
    let a: LintArgs = match args {
        Value::Null => serde_json::from_value(json!({})),
        v @ Value::Object(_) => serde_json::from_value(v.clone()),
        other => return Err(bad(format!("the arguments must be a JSON object, got {other}"))),
    }
    .map_err(|e| bad(e.to_string()))?;

    let mut opts = base.clone();
    let track = |name: &TrackRef| -> Result<TrackRef> {
        match project.track_index(name) {
            Some(i) => Ok(TrackRef::Id(project.tracks[i].id)),
            None => {
                let names: Vec<String> = project.tracks.iter().map(|t| format!("#{} {:?}", t.id, t.name)).collect();
                let list = if names.is_empty() { "the project has no tracks".to_string() } else { format!("the tracks are {}", names.join(", ")) };
                Err(bad(format!("no track {name} ({list})")))
            }
        }
    };
    if let Some(f) = a.from_bar {
        if f < 1 {
            return Err(bad("bars are 1-based: from_bar must be 1 or more".into()));
        }
        opts.from_bar = Some(f);
    }
    if let Some(t) = a.to_bar {
        if t < opts.from_bar.unwrap_or(1) {
            return Err(bad(format!("to_bar {t} is before from_bar {}", opts.from_bar.unwrap_or(1))));
        }
        opts.to_bar = Some(t);
    }
    if let Some(names) = &a.checks {
        let mut checks = Vec::new();
        for n in names {
            let all: Vec<&str> = Check::ALL.iter().map(|c| c.name()).collect();
            checks.push(Check::parse(n).ok_or_else(|| bad(format!("unknown check {n:?} (the checks are {})", all.join(", "))))?);
        }
        opts.checks = Some(checks);
    }
    if let Some(m) = &a.melody {
        opts.melody = Some(m.iter().map(&track).collect::<Result<_>>()?);
    }
    for (name, &k) in &a.transpose {
        if !(-48..=48).contains(&k) {
            return Err(bad(format!("transpose {name:?}: {k} semitones is outside -48..48")));
        }
        let r = track(&TrackRef::Name(name.clone()))?;
        opts.transpose.retain(|(t, _)| project.track_index(t) != project.track_index(&r));
        opts.transpose.push((r, k));
    }
    for (name, (lo, hi)) in &a.ranges {
        let what = format!("ranges {name:?}");
        let (lo, hi) = (lo.resolve(&what)?, hi.resolve(&what)?);
        if lo > hi {
            return Err(bad(format!("{what}: write the lowest key first ({} is above {})", crate::theory::note::note_name(lo), crate::theory::note::note_name(hi))));
        }
        let r = track(&TrackRef::Name(name.clone()))?;
        opts.ranges.retain(|(t, _, _)| project.track_index(t) != project.track_index(&r));
        opts.ranges.push((r, lo, hi));
    }
    if let Some(b) = a.bend_range {
        if !(b > 0.0 && b <= 96.0) {
            return Err(bad(format!("bend_range {b} must be above 0 and at most 96 semitones")));
        }
        opts.bend_range = b;
    }
    if let Some(m) = a.max_leap {
        if m == 0 || m > 127 {
            return Err(bad(format!("max_leap {m} must be 1-127")));
        }
        opts.max_leap = m;
    }
    if a.max_findings > 10_000 {
        return Err(bad(format!("max_findings {} is more than 10000", a.max_findings)));
    }
    let mut skipped = Vec::new();
    if let Some(dir) = &a.catalog_dir {
        let (cat, s) = InstrumentCatalog::load_dir(Path::new(dir)).map_err(|e| bad(format!("catalog_dir {dir:?}: {e}")))?;
        opts.catalog = Some(cat);
        skipped = s;
    }

    let mut report = lint(project, &opts);
    if let Some(min) = a.min_severity {
        report.findings.retain(|f| f.severity >= min);
    }
    if !skipped.is_empty() {
        let n = skipped.len();
        let mut listed: Vec<String> = skipped.into_iter().take(5).collect();
        if n > 5 {
            listed.push(format!("{} more", n - 5));
        }
        report.notes.push(format!("catalog: skipped {n} card{} ({})", if n == 1 { "" } else { "s" }, listed.join("; ")));
    }
    Ok(match a.format {
        Format::Json => report.to_json(a.max_findings),
        Format::Text => json!({"text": report.to_text(a.max_findings)}),
    })
}
