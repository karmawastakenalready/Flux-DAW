//! Lines: melody from notation, phrasing of existing notes, and the fit check against the chord track.

use super::args::{FitArgs, Fix, MelodyArgs, PhraseArgs, PhraseOpts};
use super::{Ctx, Done, add_notes, clear_notes, is_blip, note_key, range_label, starts_in};
use crate::error::{Error, Result};
use crate::notation::{MelodyOptions, bar_count, melody as lay_out};
use crate::phrase::phrase as shape;
use crate::theory::{Chord, ToneClass, classify, degree_name};
use crate::util::velocity;
use flux_core::{Command, Note, TrackRef};
use serde_json::{Value, json};

/// Metric weight from which a beat counts as strong (the downbeat, and beat 3 of 4/4).
const STRONG: f64 = 0.6;
/// The most notes the fit check lists one by one.
const MAX_LISTED: usize = 50;
/// The most fit warnings a melody call adds.
const MAX_MELODY_WARNINGS: usize = 6;

fn phrase_options(v: &Value) -> Result<Option<PhraseOpts>> {
    match v {
        Value::Bool(false) | Value::Null => Ok(None),
        Value::Bool(true) => Ok(Some(PhraseOpts::default())),
        Value::Object(_) => serde_json::from_value(v.clone()).map(Some).map_err(|e| Error::invalid(format!("phrase: {e}"))),
        other => Err(Error::invalid(format!("phrase must be true, false or an object of phrase options, got {other}"))),
    }
}

pub(crate) fn melody(cx: &mut Ctx, a: MelodyArgs) -> Result<Done> {
    let t = cx.track(&a.track)?;
    let n_bars = bar_count(&a.notation) as u32;
    if n_bars == 0 {
        return Err(Error::invalid("the notation is empty"));
    }
    let to = a.bar.saturating_add(n_bars - 1);
    let (start, end) = cx.bars(a.bar, to)?;
    let unit = a.unit.unwrap_or_else(|| (4.0 / cx.meter.den as f64).min(1.0));
    if !(1.0..=127.0).contains(&a.vel) {
        return Err(Error::invalid(format!("vel must be 1-127, got {}", a.vel)));
    }
    if !(a.legato.is_finite() && a.legato > 0.0 && a.short.is_finite() && a.short > 0.0) {
        return Err(Error::invalid("legato and short must be positive fractions of each note's length"));
    }
    let opts = MelodyOptions { unit, vel: a.vel, transpose: a.transpose, legato: a.legato, short: a.short, arc: a.arc };
    let mut notes = lay_out(&a.notation, start, cx.bar_beats, &opts)?;
    let phrased = match a.phrase.as_ref().map(phrase_options).transpose()?.flatten() {
        Some(p) => {
            notes = shape(&notes, cx.meter, &p.options()?);
            true
        }
        None => false,
    };
    if let Some(last) = notes.iter().map(|n| n.start + n.dur).reduce(f64::max)
        && last > cx.p.total_beats() + 1e-6
    {
        cx.warn(format!("the last note rings past the end of the song ({})", cx.pos(last)));
    }
    let (mut commands, removed) = if a.replace { clear_notes(t, a.bar, to, start, end) } else { (vec![], 0) };
    commands.extend(add_notes(t, &notes));
    // a quick look at how the line sits on the chord track
    cx.uses_key();
    let fit = check(cx, &notes, 0);
    let strong: Vec<&Issue> = fit.issues.iter().filter(|i| i.strong).collect();
    for i in strong.iter().take(MAX_MELODY_WARNINGS) {
        cx.warn(format!("{}: {} is {} over {} (the fit action can move it)", cx.pos(i.note.start), cx.name(i.note.pitch), i.describe(), i.chord));
    }
    if strong.len() > MAX_MELODY_WARNINGS {
        cx.warn(format!("... and {} more strong-beat clashes", strong.len() - MAX_MELODY_WARNINGS));
    }
    let mut summary = json!({
        "track": t.name,
        "bars": [a.bar, to],
        "notes": notes.len(),
        "removed": removed,
        "unit": unit,
        "phrased": phrased,
        "fit": fit.counts(),
    });
    if let (Some(lo), Some(hi)) = (notes.iter().map(|n| n.pitch).min(), notes.iter().map(|n| n.pitch).max()) {
        summary["span"] = json!([cx.name(lo), cx.name(hi)]);
    }
    Ok(Done { commands, summary, label: format!("melody on {}", range_label(t, a.bar, to)) })
}

pub(crate) fn phrase(cx: &mut Ctx, a: PhraseArgs) -> Result<Done> {
    let t = cx.track(&a.track)?;
    let (start, end) = cx.bars(a.from_bar, a.to_bar)?;
    if let Some(v) = a.vel
        && !(1.0..=127.0).contains(&v)
    {
        return Err(Error::invalid(format!("vel must be 1-127, got {v}")));
    }
    let opts = a.opts().options()?;
    let old: Vec<Note> = t.notes.iter().filter(|n| starts_in(n, start, end) && !is_blip(n)).copied().collect();
    if old.is_empty() {
        return Err(Error::invalid(format!("{} has no notes in {} to phrase", t.name, range_label(t, a.from_bar, a.to_bar))));
    }
    let base: Vec<Note> = match a.vel {
        Some(v) => old.iter().map(|n| Note { vel: velocity(v), ..*n }).collect(),
        None => old.clone(),
    };
    let new = shape(&base, cx.meter, &opts);
    let mut commands = vec![Command::DeleteNotes { track: TrackRef::Id(t.id), notes: old.iter().map(note_key).collect() }];
    commands.extend(add_notes(t, &new));
    let moved = old.len() - new.iter().filter(|n| old.iter().any(|o| o.start == n.start && o.pitch == n.pitch)).count();
    let vels: Vec<u8> = new.iter().map(|n| n.vel).collect();
    let summary = json!({
        "track": t.name,
        "bars": [a.from_bar, a.to_bar],
        "notes": new.len(),
        "base_vel": a.vel,
        "vel_range": [vels.iter().min(), vels.iter().max()],
        "moved": moved,
    });
    Ok(Done { commands, summary, label: format!("phrase {}", range_label(t, a.from_bar, a.to_bar)) })
}

/// A note the fit check flags.
struct Issue {
    note: Note,
    heard: u8,
    chord: String,
    class: ToneClass,
    degree: &'static str,
    strong: bool,
    /// The heard pitch of the nearest chord tone (a tie goes down).
    nearest: Option<u8>,
}

impl Issue {
    fn describe(&self) -> String {
        match self.class {
            ToneClass::Avoid => format!("an avoid note ({})", self.degree),
            _ => format!("outside the chord scale ({})", self.degree),
        }
    }
}

#[derive(Default)]
struct Fit {
    chord_tones: usize,
    tensions: usize,
    avoid: usize,
    outside: usize,
    /// Notes over no chord, N.C. or an unreadable symbol.
    unchecked: usize,
    issues: Vec<Issue>,
}

impl Fit {
    fn counts(&self) -> Value {
        json!({"chord_tones": self.chord_tones, "tensions": self.tensions, "avoid": self.avoid, "outside": self.outside, "unchecked": self.unchecked})
    }
}

fn nearest_chord_tone(pitch: u8, chord: &Chord) -> Option<u8> {
    (0..=6i32).flat_map(|d| [pitch as i32 - d, pitch as i32 + d]).filter(|p| (0..=127).contains(p)).find(|&p| chord.contains((p % 12) as u8)).map(|p| p as u8)
}

/// Classifies every note (not blips) against the chord sounding where it starts.
fn check(cx: &Ctx, notes: &[Note], transpose: i32) -> Fit {
    let mut fit = Fit::default();
    for n in notes.iter().filter(|n| !is_blip(n)) {
        let heard = n.pitch as i32 + transpose;
        let (Some(chord), Ok(heard)) = (cx.harmony.chord_at(n.start), u8::try_from(heard)) else {
            fit.unchecked += 1;
            continue;
        };
        let class = classify(heard % 12, chord, cx.key.as_ref());
        match class {
            ToneClass::ChordTone => fit.chord_tones += 1,
            ToneClass::Tension => fit.tensions += 1,
            ToneClass::Avoid => fit.avoid += 1,
            ToneClass::Outside => fit.outside += 1,
        }
        if matches!(class, ToneClass::Avoid | ToneClass::Outside) {
            fit.issues.push(Issue {
                note: *n,
                heard,
                chord: chord.symbol.clone(),
                class,
                degree: degree_name(heard % 12, chord),
                strong: cx.meter.weight(n.start) >= STRONG,
                nearest: nearest_chord_tone(heard, chord),
            });
        }
    }
    fit
}

pub(crate) fn fit(cx: &mut Ctx, a: FitArgs) -> Result<Done> {
    cx.uses_key();
    let t = cx.track(&a.track)?;
    let (from, to, start, end) = cx.bars_or_song(a.from_bar, a.to_bar)?;
    if cx.harmony.is_empty() {
        return Err(Error::invalid("the chord track is empty: set it with the harmony action first"));
    }
    let notes: Vec<Note> = t.notes.iter().filter(|n| starts_in(n, start, end)).copied().collect();
    let fit = check(cx, &notes, a.transpose);
    let wanted = |i: &Issue| match a.fix {
        Fix::None => false,
        Fix::Strong => i.strong,
        Fix::All => true,
    };
    let mut moves: Vec<(Note, Note)> = Vec::new();
    let mut listed = Vec::new();
    for i in &fit.issues {
        let mut v = cx.at_json(i.note.start);
        v["note"] = json!(cx.name(i.note.pitch));
        if a.transpose != 0 {
            v["sounds"] = json!(cx.name(i.heard));
        }
        v["over"] = json!(i.chord);
        v["class"] = json!(i.class);
        v["degree"] = json!(i.degree);
        v["strong"] = json!(i.strong);
        v["dur"] = json!(i.note.dur);
        let target = i.nearest.map(|h| h as i32 - a.transpose).and_then(|p| u8::try_from(p).ok()).filter(|&p| p <= 127);
        if let Some(p) = target {
            v["nearest_chord_tone"] = json!(cx.name(p));
        }
        if wanted(i) {
            match target {
                Some(p) if t.notes.iter().chain(moves.iter().map(|(_, m)| m)).any(|o| o.pitch == p && (o.start - i.note.start).abs() < 1e-3) => {
                    cx.warn(format!("{}: left {} alone: {} already sounds there", cx.pos(i.note.start), cx.name(i.note.pitch), cx.name(p)));
                }
                Some(p) => {
                    moves.push((i.note, Note { pitch: p, ..i.note }));
                    v["fixed_to"] = json!(cx.name(p));
                }
                None => cx.warn(format!("{}: no chord tone near {}", cx.pos(i.note.start), cx.name(i.note.pitch))),
            }
        }
        if listed.len() < MAX_LISTED {
            listed.push(v);
        }
    }
    let mut summary = json!({
        "track": t.name,
        "bars": [from, to],
        "transpose": a.transpose,
        "counts": fit.counts(),
        "strong_beat_issues": fit.issues.iter().filter(|i| i.strong).count(),
        "issues": listed,
        "fixed": moves.len(),
    });
    if fit.issues.len() > MAX_LISTED {
        summary["more_issues"] = json!(fit.issues.len() - MAX_LISTED);
    }
    let mut commands = Vec::new();
    if !moves.is_empty() {
        commands.push(Command::DeleteNotes { track: TrackRef::Id(t.id), notes: moves.iter().map(|(o, _)| note_key(o)).collect() });
        let new: Vec<Note> = moves.iter().map(|(_, n)| *n).collect();
        commands.extend(add_notes(t, &new));
    }
    let verb = if commands.is_empty() { "checked" } else { "fitted" };
    Ok(Done { commands, summary, label: format!("{verb} {}", range_label(t, from, to)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_tones_tie_down() {
        let c = Chord::parse("C").unwrap();
        assert_eq!(nearest_chord_tone(62, &c), Some(60)); // D: C and E are both 2 away
        assert_eq!(nearest_chord_tone(65, &c), Some(64));
        assert_eq!(nearest_chord_tone(66, &c), Some(67));
    }

    #[test]
    fn phrase_option_forms() {
        assert!(phrase_options(&json!(false)).unwrap().is_none());
        assert_eq!(phrase_options(&json!(true)).unwrap().unwrap().rit, 0.08);
        assert_eq!(phrase_options(&json!({"rit": 0})).unwrap().unwrap().rit, 0.0);
        assert!(phrase_options(&json!({"ritt": 0})).unwrap_err().to_string().contains("ritt"));
        assert!(phrase_options(&json!(3)).is_err());
    }
}
