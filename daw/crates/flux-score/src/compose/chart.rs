//! The chord track: charts written onto it (`harmony` with `chords`, and the `chords` option of the part
//! actions), and reading it back with tones, chord scales and avoid notes.

use super::args::HarmonyArgs;
use super::{Ctx, Done};
use crate::error::{Error, Result};
use crate::harmony::{ChartChord, chart_marks, parse_chart, replace_marks};
use crate::theory::note::pc_name;
use crate::theory::{Chord, Scale, ToneClass, chord_scale, classify};
use flux_core::Command;
use serde_json::{Value, json};

/// The most chords a read-back lists.
const MAX_LISTED: usize = 128;

/// Puts a chord chart on bars `[from, to]` for the rest of this call: the chart repeats to fill the range
/// (a 4-bar chart over 8 bars plays twice) and the chord that sounded after the range is restored. Returns
/// the SetHarmony command when `write` is set and the chord track changes.
pub(crate) fn apply_chart(cx: &mut Ctx, text: &str, from: u32, to: u32, write: bool) -> Result<Option<Command>> {
    let chart = parse_chart(text, cx.bar_beats)?;
    let n = chart.iter().map(|c| c.bar + 1).max().unwrap_or(0);
    if n == 0 {
        return Err(Error::invalid("the chord chart is empty"));
    }
    let span = (to - from + 1) as usize;
    if n > span {
        return Err(Error::invalid(format!(
            "the chord chart has {n} bars but bars {from}-{to} hold {span}; pass a longer range or a shorter chart"
        )));
    }
    let cycled: Vec<ChartChord> =
        (0..span.div_ceil(n)).flat_map(|rep| chart.iter().map(move |c| ChartChord { bar: rep * n + c.bar, ..c.clone() })).filter(|c| c.bar < span).collect();
    let marks = replace_marks(&cx.marks, &chart_marks(&cycled, from), from, to, cx.p.bars);
    let changed = marks != cx.marks;
    cx.set_marks(marks);
    Ok((write && changed).then(|| Command::SetHarmony { chords: cx.marks.clone() }))
}

/// The chord scale of `chord` as pitch-class names from the root, spelled in the key when it has seven
/// notes, else with the chord's accidentals.
fn spelled(pcs: impl Iterator<Item = u8>, chord: &Chord, key: Option<&Scale>) -> Vec<String> {
    pcs.map(|pc| match key {
        Some(k) if k.mode.is_heptatonic() => k.spell(pc),
        _ => pc_name(pc, chord.prefers_flats()).to_string(),
    })
    .collect()
}

pub(crate) fn chord_json(chord: &Chord, key: Option<&Scale>) -> Value {
    let set = chord_scale(chord, key);
    let from_root = || (0..12u8).map(|i| (chord.root + i) % 12).filter(|pc| set.contains(*pc));
    let of_class = |class: ToneClass| spelled(from_root().filter(|&pc| classify(pc, chord, key) == class), chord, key);
    json!({
        "tones": chord.tone_names(),
        "bass": chord.bass_name(),
        "scale": spelled(from_root(), chord, key).join(" "),
        "tensions": of_class(ToneClass::Tension),
        "avoid": of_class(ToneClass::Avoid),
    })
}

pub(crate) fn harmony(cx: &mut Ctx, a: HarmonyArgs) -> Result<Done> {
    if let Some(text) = &a.chords {
        let from = a.from_bar.unwrap_or(1);
        let to = match a.to_bar {
            Some(t) => t,
            None => {
                let n = parse_chart(text, cx.bar_beats)?.iter().map(|c| c.bar as u32 + 1).max().unwrap_or(1);
                from + n - 1
            }
        };
        cx.bars(from, to)?;
        let before = cx.marks.len();
        let cmd = apply_chart(cx, text, from, to, true)?;
        let label = if from == to { format!("chords for bar {from}") } else { format!("chords for bars {from}-{to}") };
        let marks: Vec<Value> = cx.marks.iter().filter(|m| m.bar >= from && m.bar <= to + 1).map(|m| json!({"bar": m.bar, "beat": m.beat, "chord": m.chord})).collect();
        let summary = json!({
            "bars": [from, to],
            "changed": cmd.is_some(),
            "marks": marks,
            "marks_in_track": cx.marks.len(),
            "marks_before": before,
        });
        return Ok(Done { commands: cmd.into_iter().collect(), summary, label });
    }
    // read back
    cx.uses_key();
    let (from, to, start, end) = cx.bars_or_song(a.from_bar, a.to_bar)?;
    let key = cx.key;
    let spans = cx.harmony.spans_in(start, end);
    let mut listed = Vec::new();
    for s in spans.iter().take(MAX_LISTED) {
        let mut v = json!({"bar": s.bar, "beat": s.beat, "chord": s.text, "until": cx.at_json(s.end)});
        match &s.parsed {
            Ok(sym) => {
                if let Some(c) = sym.chord() {
                    if let (Value::Object(m), Value::Object(extra)) = (&mut v, chord_json(c, key.as_ref())) {
                        m.extend(extra);
                    }
                } else {
                    v["no_chord"] = json!(true);
                }
            }
            Err(e) => v["error"] = json!(e.to_string()),
        }
        listed.push(v);
    }
    let first = spans.first().map_or(end, |s| s.start);
    let mut summary = json!({
        "bars": [from, to],
        "key": key.map(|k| k.to_string()),
        "chords": listed,
    });
    if spans.len() > MAX_LISTED {
        summary["more"] = json!(spans.len() - MAX_LISTED);
    }
    if first > start + 1e-9 {
        summary["no_harmony_until"] = cx.at_json(first);
        cx.warn(format!("the chord track has no chord before {}", cx.pos(first)));
    }
    let errors = spans.iter().filter(|s| s.parsed.is_err()).count();
    if errors > 0 {
        cx.warn(format!("{errors} chord mark(s) could not be read; composing tools stop at them"));
    }
    let label = format!("read the chord track, bars {from}-{to}");
    Ok(Done { commands: vec![], summary, label })
}
