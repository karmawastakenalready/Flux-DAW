//! Rhythm patterns for chords, bass lines and arpeggios, written one character per step.
//!
//! ```text
//! x--x --x-        chords: strike, hold, hold, strike, hold, hold, strike, hold (step 0.5 in 4/4)
//! x.o. 5.o.        bass: root, rest, octave, rest, fifth, rest, octave, rest
//! 0123 2101 | 0-x- arpeggio: chord tones by index (0 = lowest), two bars that alternate
//! ```
//!
//! * Each character lasts `step` beats. Spaces are ignored. `|` separates bars: the bar patterns are used in
//!   turn from the first bar of the range and cycle. A pattern shorter than its bar repeats; a longer one
//!   is cut at the barline. Every bar starts its pattern afresh.
//! * `x` strikes (the chord, the bass note, or for arpeggios the whole chord), `X` strikes with an accent,
//!   `-` holds the last strike, `.` is a rest.
//! * Bass patterns also have `o`/`O` (the bass an octave up) and `5` (the chord's fifth above the bass).
//! * Arpeggio patterns have digits `0`-`9`: chord tones from the bottom of the voicing, continuing an octave
//!   up past the top (index 4 of a 4-note voicing is the lowest note an octave higher).
//!
//! At a chord change a held note is struck again on the new chord; "N.C." silences everything; a hold
//! with nothing sounding (at the start, after a rest or after N.C.) is a rest.

use crate::error::TextError;
use crate::harmony::ChordSpan;

/// What a pattern is for: decides which characters are allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternKind {
    Chords,
    Bass,
    Arpeggio,
}

/// What a strike plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Strike {
    /// The whole voicing.
    Chord,
    /// The bass note.
    Bass,
    /// The bass note an octave up.
    Octave,
    /// The chord's fifth above the bass note.
    Fifth,
    /// The n-th tone of the voicing from the bottom (wrapping up by octaves).
    Tone(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Strike(Strike, bool),
    Hold,
    Rest,
}

/// A parsed pattern.
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    bars: Vec<Vec<Step>>,
    /// Beats per character.
    pub step: f64,
}

/// One note-on of a rendered pattern: from `start` until `end` (before gating).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PatternEvent {
    pub start: f64,
    pub end: f64,
    pub strike: Strike,
    /// Written in upper case.
    pub accent: bool,
    /// Index of the chord span (in the slice given to [`render`]) it sounds over.
    pub span: usize,
}

fn err(position: usize, token: &str, message: impl Into<String>) -> TextError {
    TextError { what: "pattern", position, token: token.to_string(), message: message.into() }
}

impl Pattern {
    /// Parses a pattern of `kind` with `step` beats per character.
    pub fn parse(text: &str, kind: PatternKind, step: f64) -> Result<Pattern, TextError> {
        if !(step.is_finite() && step > 0.0) {
            return Err(err(0, text, format!("step must be a positive number of beats, got {step}")));
        }
        let mut bars: Vec<Vec<Step>> = vec![vec![]];
        for (i, c) in text.chars().enumerate() {
            let s = match (c, kind) {
                (c, _) if c.is_whitespace() => continue,
                ('|', _) => {
                    bars.push(vec![]);
                    continue;
                }
                ('-', _) => Step::Hold,
                ('.', _) => Step::Rest,
                ('x' | 'X', PatternKind::Chords | PatternKind::Arpeggio) => Step::Strike(Strike::Chord, c == 'X'),
                ('x' | 'X', PatternKind::Bass) => Step::Strike(Strike::Bass, c == 'X'),
                ('o' | 'O', PatternKind::Bass) => Step::Strike(Strike::Octave, c == 'O'),
                ('5', PatternKind::Bass) => Step::Strike(Strike::Fifth, false),
                (d, PatternKind::Arpeggio) if d.is_ascii_digit() => Step::Strike(Strike::Tone(d as u8 - b'0'), false),
                _ => {
                    let allowed = match kind {
                        PatternKind::Chords => "x X - . |",
                        PatternKind::Bass => "x X o O 5 - . |",
                        PatternKind::Arpeggio => "0-9 x X - . |",
                    };
                    return Err(err(i, &c.to_string(), format!("unknown pattern character (use {allowed})")));
                }
            };
            bars.last_mut().expect("bars starts non-empty").push(s);
        }
        if let Some(i) = bars.iter().position(Vec::is_empty) {
            let msg = if bars.len() == 1 { "the pattern is empty".to_string() } else { format!("bar {} of the pattern is empty", i + 1) };
            return Err(err(0, text, msg));
        }
        Ok(Pattern { bars, step })
    }

    /// A pattern that strikes once per chord and holds it.
    fn step_at(&self, bar_index: usize, k: usize) -> Step {
        let bar = &self.bars[bar_index % self.bars.len()];
        bar[k % bar.len()]
    }
}

/// Renders a pattern over chord spans covering `[from, to)` (bars of `bar_beats` beats starting at beat 0;
/// `from` should be a downbeat). Without a pattern, each chord is struck once with `default` and held to
/// its end. Spans without a chord ("N.C.", unreadable) are silent.
pub fn render(pattern: Option<&Pattern>, spans: &[ChordSpan], from: f64, to: f64, bar_beats: f64, default: Strike) -> Vec<PatternEvent> {
    let mut out: Vec<PatternEvent> = Vec::new();
    let Some(p) = pattern else {
        for (i, s) in spans.iter().enumerate() {
            if s.chord().is_some() && s.end > s.start {
                out.push(PatternEvent { start: s.start, end: s.end, strike: default, accent: false, span: i });
            }
        }
        return out;
    };
    // time points: grid steps of every bar in the range, plus chord changes
    let first_bar = (from / bar_beats + 1e-9).floor();
    let mut points: Vec<(f64, Option<(usize, usize)>)> = Vec::new(); // (time, (bar index from range start, step))
    let mut b = 0usize;
    loop {
        let bar_start = (first_bar + b as f64) * bar_beats;
        if bar_start >= to - 1e-9 {
            break;
        }
        let mut k = 0usize;
        loop {
            let t = bar_start + k as f64 * p.step;
            if t >= bar_start + bar_beats - 1e-9 || t >= to - 1e-9 {
                break;
            }
            if t >= from - 1e-9 {
                points.push((t, Some((b, k))));
            }
            k += 1;
        }
        b += 1;
    }
    for s in spans {
        if !points.iter().any(|(t, _)| (t - s.start).abs() < 1e-9) {
            points.push((s.start, None));
        }
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0));

    let step_of = |t: f64| -> Step {
        let rel = t / bar_beats - first_bar;
        let bar = (rel + 1e-9).floor().max(0.0) as usize;
        let k = ((t - (first_bar + bar as f64) * bar_beats) / p.step + 1e-9).floor().max(0.0) as usize;
        p.step_at(bar, k)
    };
    let span_at = |t: f64| spans.iter().position(|s| s.start <= t + 1e-9 && t < s.end - 1e-9);

    let mut sounding: Option<usize> = None; // index into out
    let mut current_span: Option<usize> = None;
    for (t, grid) in points {
        let span = span_at(t);
        let changed = span != current_span;
        current_span = span;
        let step = match grid {
            Some((b, k)) => p.step_at(b, k),
            // a chord change between steps: what the step it falls in is doing continues
            None => match step_of(t) {
                Step::Rest => Step::Rest,
                _ => Step::Hold,
            },
        };
        let held = sounding.map(|i| out[i].strike);
        let end_current = |out: &mut Vec<PatternEvent>, sounding: &mut Option<usize>| {
            if let Some(i) = sounding.take() {
                out[i].end = t;
            }
        };
        let active = span.is_some_and(|i| spans[i].chord().is_some());
        if !active {
            end_current(&mut out, &mut sounding);
            continue;
        }
        let i = span.expect("active implies a span");
        match step {
            Step::Strike(strike, accent) => {
                end_current(&mut out, &mut sounding);
                out.push(PatternEvent { start: t, end: t, strike, accent, span: i });
                sounding = Some(out.len() - 1);
            }
            Step::Hold if changed => {
                end_current(&mut out, &mut sounding);
                if let Some(strike) = held {
                    out.push(PatternEvent { start: t, end: t, strike, accent: false, span: i });
                    sounding = Some(out.len() - 1);
                }
            }
            Step::Hold => {}
            Step::Rest => end_current(&mut out, &mut sounding),
        }
    }
    if let Some(i) = sounding {
        out[i].end = spans[out[i].span].end.min(to);
    }
    // a note never outlasts its chord span
    for e in &mut out {
        e.end = e.end.min(spans[e.span].end);
    }
    out.retain(|e| e.end > e.start + 1e-9);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harmony::ChordTrack;
    use flux_core::ChordMark;

    fn spans(marks: &[(u32, f64, &str)], bars: u32) -> Vec<ChordSpan> {
        let m: Vec<ChordMark> = marks.iter().map(|&(bar, beat, c)| ChordMark { bar, beat, chord: c.into() }).collect();
        ChordTrack::from_marks(&m, 4.0, bars as f64 * 4.0).spans_in(0.0, bars as f64 * 4.0)
    }

    fn ev(e: &[PatternEvent]) -> Vec<(f64, f64, Strike)> {
        e.iter().map(|e| (e.start, e.end, e.strike)).collect()
    }

    #[test]
    fn default_strikes_once_per_chord() {
        let s = spans(&[(1, 0.0, "C"), (2, 0.0, "N.C."), (3, 0.0, "G")], 3);
        let e = render(None, &s, 0.0, 12.0, 4.0, Strike::Chord);
        assert_eq!(ev(&e), [(0.0, 4.0, Strike::Chord), (8.0, 12.0, Strike::Chord)]);
    }

    #[test]
    fn strikes_holds_and_rests() {
        let s = spans(&[(1, 0.0, "C")], 2);
        let p = Pattern::parse("x-.X", PatternKind::Chords, 1.0).unwrap();
        let e = render(Some(&p), &s, 0.0, 8.0, 4.0, Strike::Chord);
        assert_eq!(ev(&e), [(0.0, 2.0, Strike::Chord), (3.0, 4.0, Strike::Chord), (4.0, 6.0, Strike::Chord), (7.0, 8.0, Strike::Chord)]);
        assert!(e[1].accent && !e[0].accent);
    }

    #[test]
    fn chord_changes_restrike_holds() {
        let s = spans(&[(1, 0.0, "C"), (1, 2.5, "F")], 1);
        let p = Pattern::parse("x---", PatternKind::Chords, 1.0).unwrap();
        let e = render(Some(&p), &s, 0.0, 4.0, 4.0, Strike::Chord);
        assert_eq!(ev(&e), [(0.0, 2.5, Strike::Chord), (2.5, 4.0, Strike::Chord)]);
        assert_eq!(e[1].span, 1);
        // a rest step at the change stays silent
        let p = Pattern::parse("x-..", PatternKind::Chords, 1.0).unwrap();
        let e = render(Some(&p), &s, 0.0, 4.0, 4.0, Strike::Chord);
        assert_eq!(ev(&e), [(0.0, 2.0, Strike::Chord)]);
    }

    #[test]
    fn bar_patterns_cycle_and_short_patterns_repeat() {
        let s = spans(&[(1, 0.0, "C")], 3);
        let p = Pattern::parse("x. | .5", PatternKind::Bass, 2.0).unwrap();
        let e = render(Some(&p), &s, 0.0, 12.0, 4.0, Strike::Bass);
        assert_eq!(ev(&e), [(0.0, 2.0, Strike::Bass), (6.0, 8.0, Strike::Fifth), (8.0, 10.0, Strike::Bass)]);
        let p = Pattern::parse("01", PatternKind::Arpeggio, 1.0).unwrap();
        let e = render(Some(&p), &s, 0.0, 4.0, 4.0, Strike::Chord);
        assert_eq!(e.iter().map(|e| e.strike).collect::<Vec<_>>(), [Strike::Tone(0), Strike::Tone(1), Strike::Tone(0), Strike::Tone(1)]);
    }

    #[test]
    fn holds_with_nothing_sounding_are_rests() {
        let s = spans(&[(1, 0.0, "C")], 1);
        let p = Pattern::parse("-x--", PatternKind::Chords, 1.0).unwrap();
        let e = render(Some(&p), &s, 0.0, 4.0, 4.0, Strike::Chord);
        assert_eq!(ev(&e), [(1.0, 4.0, Strike::Chord)]);
    }

    #[test]
    fn parse_errors() {
        let e = Pattern::parse("x-o-", PatternKind::Chords, 1.0).unwrap_err();
        assert_eq!(e.position, 2);
        assert!(e.message.contains("x X - . |"));
        assert!(Pattern::parse("  ", PatternKind::Chords, 1.0).unwrap_err().message.contains("empty"));
        assert!(Pattern::parse("x||x", PatternKind::Chords, 1.0).unwrap_err().message.contains("bar 2"));
        assert!(Pattern::parse("x", PatternKind::Chords, 0.0).is_err());
        assert!(Pattern::parse("0123 x-", PatternKind::Arpeggio, 0.5).is_ok());
    }
}
