//! Parts written from the chord track: voice-led chords, bass lines and arpeggios, each in a rhythm
//! pattern (see [`crate::pattern`]).

use super::args::{ArpeggioArgs, BassArgs, ChordsArgs, PitchArg, Pitches};
use super::chart::apply_chart;
use super::{Ctx, Done, add_notes, clear_notes, is_blip, range_label};
use crate::error::{Error, Result};
use crate::harmony::ChordSpan;
use crate::pattern::{Pattern, PatternEvent, PatternKind, Strike, render};
use crate::theory::{Chord, ChordSymbol};
use crate::util::{beat4, velocity};
use crate::voicing::{BassLineOptions, VoiceLeadOptions, VoiceLeading, bass_line, voice_lead};
use flux_core::{Command, Note, Track};
use serde_json::{Value, json};

/// The most chords a summary lists one by one.
const MAX_LISTED: usize = 64;

/// What every part action shares: the track, the range, the chord spans and the commands that go first.
struct Setup<'a> {
    track: &'a Track,
    from: u32,
    to: u32,
    a: f64,
    b: f64,
    /// Chord spans over the range, repeats merged.
    spans: Vec<ChordSpan>,
    /// For each span, the index of its chord among the chords (None for N.C.).
    chord_index: Vec<Option<usize>>,
    chords: Vec<Chord>,
    /// SetHarmony (when a chart was given) and the deletion of the notes being replaced.
    pre: Vec<Command>,
    removed: usize,
}

#[derive(Clone, Copy)]
struct Common<'s> {
    from: u32,
    to: u32,
    chart: Option<&'s str>,
    set_harmony: bool,
    replace: bool,
}

fn setup<'a>(cx: &mut Ctx<'a>, track: &flux_core::TrackRef, c: Common) -> Result<Setup<'a>> {
    let t = cx.track(track)?;
    let (a, b) = cx.bars(c.from, c.to)?;
    let mut pre = Vec::new();
    if let Some(text) = c.chart {
        pre.extend(apply_chart(cx, text, c.from, c.to, c.set_harmony)?);
    }
    let spans = merge_repeats(cx.harmony.require(a, b)?);
    let mut chords = Vec::new();
    let chord_index = spans
        .iter()
        .map(|s| {
            s.chord().map(|ch| {
                chords.push(ch.clone());
                chords.len() - 1
            })
        })
        .collect();
    let mut removed = 0;
    if c.replace {
        let (cmds, n) = clear_notes(t, c.from, c.to, a, b);
        pre.extend(cmds);
        removed = n;
    }
    Ok(Setup { track: t, from: c.from, to: c.to, a, b, spans, chord_index, chords, pre, removed })
}

/// Joins neighbouring spans that hold the same chord (the same tones over the same bass), so a held
/// chord is not struck again just because the chord track repeats it.
fn merge_repeats(spans: Vec<ChordSpan>) -> Vec<ChordSpan> {
    fn same(a: &ChordSpan, b: &ChordSpan) -> bool {
        match (&a.parsed, &b.parsed) {
            (Ok(ChordSymbol::NoChord), Ok(ChordSymbol::NoChord)) => true,
            (Ok(ChordSymbol::Chord(x)), Ok(ChordSymbol::Chord(y))) => x.bass == y.bass && x.pcs().eq(y.pcs()),
            _ => false,
        }
    }
    let mut out: Vec<ChordSpan> = Vec::with_capacity(spans.len());
    for s in spans {
        match out.last_mut() {
            Some(last) if same(last, &s) && (last.end - s.start).abs() < 1e-9 => last.end = s.end,
            _ => out.push(s),
        }
    }
    out
}

fn pitch_or(p: &Option<PitchArg>, what: &str, default: u8) -> Result<u8> {
    p.as_ref().map_or(Ok(default), |p| p.resolve(what))
}

fn range(low: &Option<PitchArg>, high: &Option<PitchArg>, defaults: (u8, u8)) -> Result<(u8, u8)> {
    let (lo, hi) = (pitch_or(low, "low", defaults.0)?, pitch_or(high, "high", defaults.1)?);
    if lo >= hi {
        return Err(Error::invalid(format!("low ({lo}) must be below high ({hi})")));
    }
    Ok((lo, hi))
}

/// The default pattern step: a beat in x/4 (or x/2), an eighth in x/8, a sixteenth in x/16.
fn default_step(cx: &Ctx) -> f64 {
    (4.0 / cx.meter.den as f64).min(1.0)
}

fn pattern(text: &Option<String>, kind: PatternKind, step: f64) -> Result<Option<Pattern>> {
    text.as_deref().map(|t| Pattern::parse(t, kind, step)).transpose().map_err(Error::from)
}

fn check_shape(gate: f64, step: f64, vel: f64) -> Result<()> {
    if !(gate.is_finite() && gate > 0.0 && gate <= 1.0) {
        return Err(Error::invalid(format!("gate must be in (0, 1] (the fraction of each step that sounds), got {gate}")));
    }
    if !(step.is_finite() && step > 0.0) {
        return Err(Error::invalid(format!("step must be a positive number of beats, got {step}")));
    }
    if !(1.0..=127.0).contains(&vel) {
        return Err(Error::invalid(format!("vel must be 1-127, got {vel}")));
    }
    Ok(())
}

/// The velocity of a pattern event: `vel`, plus `accent` scaled by the metric weight (downbeats louder),
/// plus `accent` again for an accented (upper-case) strike.
fn event_vel(cx: &Ctx, vel: f64, accent: f64, e: &PatternEvent) -> u8 {
    velocity(vel + accent * (cx.meter.weight(e.start) - 0.3) + if e.accent { accent } else { 0.0 })
}

fn event_note(cx: &Ctx, e: &PatternEvent, pitch: u8, vel: f64, accent: f64, gate: f64) -> Note {
    Note { start: beat4(e.start), dur: beat4(((e.end - e.start) * gate).max(0.02)), pitch, vel: event_vel(cx, vel, accent, e) }
}

/// The last chord struck on `t` before beat `a`: the pitches of the notes starting with the latest one.
fn last_struck(t: &Track, a: f64) -> Option<(f64, Vec<u8>)> {
    let before: Vec<&Note> = t.notes.iter().filter(|n| n.start < a - 1e-9 && !is_blip(n)).collect();
    let last = before.iter().map(|n| n.start).fold(f64::NEG_INFINITY, f64::max);
    if !last.is_finite() {
        return None;
    }
    let mut ps: Vec<u8> = before.iter().filter(|n| (n.start - last).abs() < 0.01).map(|n| n.pitch).collect();
    ps.sort_unstable();
    ps.dedup();
    Some((last, ps))
}

/// The lowest note of `t` sounding when the span starts, else the lowest one starting inside it.
fn bass_under(t: &Track, s: &ChordSpan) -> Option<u8> {
    let sounding = t.notes.iter().filter(|n| !is_blip(n) && n.start <= s.start + 1e-6 && n.start + n.dur > s.start + 1e-6);
    sounding.map(|n| n.pitch).min().or_else(|| t.notes.iter().filter(|n| !is_blip(n) && n.start > s.start && n.start < s.end - 1e-9).map(|n| n.pitch).min())
}

/// Resolves the voicing start: given, or the last chord on the track when it has `voices` notes.
fn start_voicing(cx: &Ctx, given: &Option<Pitches>, t: &Track, a: f64, voices: usize) -> Result<(Option<Vec<u8>>, Value)> {
    if let Some(p) = given {
        let ps = p.resolve("start")?;
        return Ok((Some(ps), json!("given")));
    }
    match last_struck(t, a) {
        Some((at, ps)) if ps.len() == voices => {
            let desc = json!(format!("led on from the chord at {}", cx.pos(at)));
            Ok((Some(ps), desc))
        }
        _ => Ok((None, Value::Null)),
    }
}

/// Voice-leads the chords and rewrites the warnings with bar positions.
fn lead(cx: &mut Ctx, s: &Setup, opts: &VoiceLeadOptions) -> Result<VoiceLeading> {
    let vl = voice_lead(&s.chords, opts)?;
    let span_of_chord: Vec<usize> = s.chord_index.iter().enumerate().filter_map(|(i, c)| c.map(|_| i)).collect();
    for (w, &k) in vl.warnings.iter().zip(&vl.warning_chords) {
        // a single voice with no bass "moves in octaves" with itself: not a warning worth giving
        if opts.voices == 1 && opts.bass.is_none() && w.contains("parallel") {
            continue;
        }
        let at = span_of_chord.get(k).map_or_else(String::new, |&i| format!("{}: ", cx.pos(s.spans[i].start)));
        cx.warn(format!("{at}{w}"));
    }
    Ok(vl)
}

fn chord_list(cx: &Ctx, s: &Setup, pitches: impl Fn(usize) -> Vec<u8>) -> Value {
    let mut list: Vec<Value> = Vec::new();
    for (i, span) in s.spans.iter().enumerate().take(MAX_LISTED) {
        let mut v = cx.at_json(span.start);
        v["chord"] = json!(span.text);
        match s.chord_index[i] {
            Some(k) => v["notes"] = json!(pitches(k).into_iter().map(|p| cx.name(p)).collect::<Vec<_>>()),
            None => v["silent"] = json!(true),
        }
        list.push(v);
    }
    json!(list)
}

fn finish(cx: &Ctx, s: Setup, notes: Vec<Note>, mut summary: Value, what: &str) -> Done {
    summary["track"] = json!(s.track.name);
    summary["bars"] = json!([s.from, s.to]);
    summary["notes"] = json!(notes.len());
    summary["removed"] = json!(s.removed);
    if s.spans.len() > MAX_LISTED {
        summary["more_chords"] = json!(s.spans.len() - MAX_LISTED);
    }
    if let (Some(lo), Some(hi)) = (notes.iter().map(|n| n.pitch).min(), notes.iter().map(|n| n.pitch).max()) {
        summary["span"] = json!([cx.name(lo), cx.name(hi)]);
    }
    let mut commands = s.pre;
    commands.extend(add_notes(s.track, &notes));
    Done { commands, summary, label: format!("{what} on {}", range_label(s.track, s.from, s.to)) }
}

pub(crate) fn chords(cx: &mut Ctx, a: ChordsArgs) -> Result<Done> {
    let step = a.step.unwrap_or_else(|| default_step(cx));
    check_shape(a.gate, step, a.vel)?;
    let (low, high) = range(&a.low, &a.high, (48, 79))?;
    let pat = pattern(&a.pattern, PatternKind::Chords, step)?;
    let common = Common { from: a.from_bar, to: a.to_bar, chart: a.chords.as_deref(), set_harmony: a.set_harmony, replace: a.replace };
    let s = setup(cx, &a.track, common)?;
    let bass = match &a.bass_track {
        Some(r) => {
            let bt = cx.track(r)?;
            let under: Vec<Option<u8>> = s.spans.iter().zip(&s.chord_index).filter(|(_, k)| k.is_some()).map(|(sp, _)| bass_under(bt, sp)).collect();
            if let Some(i) = under.iter().position(Option::is_none) {
                let span = s.spans.iter().zip(&s.chord_index).filter(|(_, k)| k.is_some()).nth(i).map(|(sp, _)| sp.start).unwrap_or(s.a);
                cx.warn(format!("{} has no note under the chord at {}; voicing without the bass track", bt.name, cx.pos(span)));
                None
            } else {
                Some(under.into_iter().flatten().collect::<Vec<u8>>())
            }
        }
        None => None,
    };
    let (start, start_desc) = start_voicing(cx, &a.start, s.track, s.a, a.voices)?;
    let opts = VoiceLeadOptions { voices: a.voices, low, high, start, bass: bass.clone() };
    let vl = lead(cx, &s, &opts)?;
    let events = render(pat.as_ref(), &s.spans, s.a, s.b, cx.bar_beats, Strike::Chord);
    let mut notes = Vec::new();
    for e in &events {
        let Some(k) = s.chord_index[e.span] else { continue };
        for &p in &vl.pitches[k] {
            notes.push(event_note(cx, e, p, a.vel, a.accent, a.gate));
        }
    }
    let summary = json!({
        "voices": a.voices,
        "range": [cx.name(low), cx.name(high)],
        "start": start_desc,
        "above_bass_track": bass.is_some(),
        "pattern": a.pattern,
        "step": step,
        "chords": chord_list(cx, &s, |k| vl.pitches[k].clone()),
    });
    Ok(finish(cx, s, notes, summary, "chords"))
}

/// The chord tone nearest a fifth above `bass` (the fifth itself when the chord has one there).
fn fifth_above(bass: u8, chord: &Chord) -> Option<u8> {
    let target = bass as i32 + 7;
    (bass as i32 + 1..=bass as i32 + 12).filter(|p| chord.contains((p % 12) as u8)).min_by_key(|p| ((p - target).abs(), *p)).and_then(|p| u8::try_from(p).ok()).filter(|&p| p <= 127)
}

pub(crate) fn bass(cx: &mut Ctx, a: BassArgs) -> Result<Done> {
    let step = a.step.unwrap_or_else(|| default_step(cx));
    check_shape(a.gate, step, a.vel)?;
    let (low, high) = range(&a.low, &a.high, (36, 55))?;
    let pat = pattern(&a.pattern, PatternKind::Bass, step)?;
    let common = Common { from: a.from_bar, to: a.to_bar, chart: a.chords.as_deref(), set_harmony: a.set_harmony, replace: a.replace };
    let s = setup(cx, &a.track, common)?;
    let (start, start_desc) = match &a.start {
        Some(p) => {
            let ps = p.resolve("start")?;
            if ps.len() != 1 {
                return Err(Error::invalid(format!("start: a bass line leads on from one note, got {}", ps.len())));
            }
            (Some(ps[0]), json!("given"))
        }
        None => match last_struck(s.track, s.a) {
            Some((at, ps)) => (ps.first().copied(), json!(format!("led on from the note at {}", cx.pos(at)))),
            None => (None, Value::Null),
        },
    };
    let line = bass_line(&s.chords, &BassLineOptions { low, high, start, inversions: a.inversions })?;
    let events = render(pat.as_ref(), &s.spans, s.a, s.b, cx.bar_beats, Strike::Bass);
    let mut notes = Vec::new();
    let mut dropped = 0;
    for e in &events {
        let Some(k) = s.chord_index[e.span] else { continue };
        let root = line[k];
        let pitch = match e.strike {
            Strike::Octave => root.checked_add(12).filter(|&p| p <= 127),
            Strike::Fifth => fifth_above(root, &s.chords[k]),
            _ => Some(root),
        };
        match pitch {
            Some(p) => notes.push(event_note(cx, e, p, a.vel, a.accent, a.gate)),
            None => dropped += 1,
        }
    }
    if dropped > 0 {
        cx.warn(format!("{dropped} strike(s) would go above MIDI 127 and were left out"));
    }
    let summary = json!({
        "range": [cx.name(low), cx.name(high)],
        "inversions": a.inversions,
        "start": start_desc,
        "pattern": a.pattern,
        "step": step,
        "chords": chord_list(cx, &s, |k| vec![line[k]]),
    });
    Ok(finish(cx, s, notes, summary, "bass"))
}

/// Up and down through the voicing: "012321" for four voices.
fn zigzag(voices: usize) -> String {
    let up: Vec<usize> = (0..voices.min(10)).collect();
    let down = up.iter().rev().skip(1).take(up.len().saturating_sub(2));
    up.iter().chain(down).map(|i| char::from(b'0' + *i as u8)).collect()
}

pub(crate) fn arpeggio(cx: &mut Ctx, a: ArpeggioArgs) -> Result<Done> {
    let step = a.step.unwrap_or_else(|| default_step(cx) / 2.0);
    check_shape(a.gate, step, a.vel)?;
    let (low, high) = range(&a.low, &a.high, (48, 79))?;
    let text = a.pattern.clone().unwrap_or_else(|| zigzag(a.voices));
    let pat = Pattern::parse(&text, PatternKind::Arpeggio, step)?;
    let common = Common { from: a.from_bar, to: a.to_bar, chart: a.chords.as_deref(), set_harmony: a.set_harmony, replace: a.replace };
    let s = setup(cx, &a.track, common)?;
    let (start, start_desc) = match &a.start {
        Some(p) => (Some(p.resolve("start")?), json!("given")),
        None => (None, Value::Null),
    };
    let opts = VoiceLeadOptions { voices: a.voices, low, high, start, bass: None };
    let vl = lead(cx, &s, &opts)?;
    let events = render(Some(&pat), &s.spans, s.a, s.b, cx.bar_beats, Strike::Chord);
    let mut notes = Vec::new();
    let mut dropped = 0;
    for e in &events {
        let Some(k) = s.chord_index[e.span] else { continue };
        let v = &vl.pitches[k];
        let pitches: Vec<Option<u8>> = match e.strike {
            Strike::Tone(i) => {
                let (i, n) = (i as usize, v.len());
                vec![u8::try_from(v[i % n] as usize + 12 * (i / n)).ok().filter(|&p| p <= 127)]
            }
            _ => v.iter().map(|&p| Some(p)).collect(),
        };
        for p in pitches {
            match p {
                Some(p) => notes.push(event_note(cx, e, p, a.vel, a.accent, a.gate)),
                None => dropped += 1,
            }
        }
    }
    if dropped > 0 {
        cx.warn(format!("{dropped} arpeggio note(s) would go above MIDI 127 and were left out"));
    }
    let summary = json!({
        "voices": a.voices,
        "range": [cx.name(low), cx.name(high)],
        "start": start_desc,
        "pattern": text,
        "step": step,
        "chords": chord_list(cx, &s, |k| vl.pitches[k].clone()),
    });
    Ok(finish(cx, s, notes, summary, "arpeggio"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzags() {
        assert_eq!(zigzag(4), "012321");
        assert_eq!(zigzag(3), "0121");
        assert_eq!(zigzag(2), "01");
        assert_eq!(zigzag(1), "0");
    }

    #[test]
    fn fifths_above_the_bass() {
        let c = Chord::parse("C").unwrap();
        assert_eq!(fifth_above(48, &c), Some(55));
        // C/E: the nearest chord tone to B above E is C
        let ce = Chord::parse("C/E").unwrap();
        assert_eq!(fifth_above(52, &ce), Some(60));
        assert_eq!(fifth_above(127, &c), None);
    }
}
