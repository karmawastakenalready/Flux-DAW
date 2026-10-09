//! The chord track (`Project::harmony`) as time spans, and chord charts written as text.
//!
//! A chord mark holds from its bar and beat until the next mark; the last one holds to the end of the
//! song. Before the first mark there is no harmony. "N.C." marks silence the harmony.
//!
//! # Chord charts
//!
//! ```text
//! Am | F C | G@0 G/B@2.5 | N.C. | %
//! ```
//!
//! One `|`-separated cell per bar. The chords in a bar split it evenly unless one gives its own start beat
//! with `@` (0-based, in beats). `%` repeats the previous bar. Symbols with spaces inside parentheses
//! ("C7(b9 #11)") stay one chord.

use crate::error::{ChordError, Error, Result, TextError};
use crate::theory::chord::{Chord, ChordSymbol};
use flux_core::{ChordMark, Project};

/// One span of the chord track.
#[derive(Clone, Debug, PartialEq)]
pub struct ChordSpan {
    /// Absolute start beat (inclusive).
    pub start: f64,
    /// Absolute end beat (exclusive): the next mark, or the end of the song.
    pub end: f64,
    /// The mark's bar (1-based) and beat within it.
    pub bar: u32,
    pub beat: f64,
    /// The symbol as written on the chord track.
    pub text: String,
    /// The parsed symbol, or why it could not be read.
    pub parsed: Result<ChordSymbol, ChordError>,
}

impl ChordSpan {
    /// The chord, if the span holds a readable chord (not "N.C.").
    pub fn chord(&self) -> Option<&Chord> {
        self.parsed.as_ref().ok().and_then(ChordSymbol::chord)
    }

    pub fn is_no_chord(&self) -> bool {
        matches!(self.parsed, Ok(ChordSymbol::NoChord))
    }
}

/// The chord track of a project, as spans in time order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChordTrack {
    pub spans: Vec<ChordSpan>,
    bar_beats: f64,
}

impl ChordTrack {
    /// The project's chord track. Unreadable symbols do not fail here: their spans carry the error (see
    /// [`ChordTrack::errors`]) so one bad mark does not block work elsewhere in the song.
    pub fn from_project(project: &Project) -> ChordTrack {
        ChordTrack::from_marks(&project.harmony, project.beats_per_bar(), project.total_beats())
    }

    /// A chord track from marks (sorted here by bar and beat), a bar length and the song end in beats.
    pub fn from_marks(marks: &[ChordMark], bar_beats: f64, song_end: f64) -> ChordTrack {
        let mut sorted: Vec<&ChordMark> = marks.iter().collect();
        sorted.sort_by(|a, b| (a.bar, a.beat).partial_cmp(&(b.bar, b.beat)).unwrap_or(std::cmp::Ordering::Equal));
        let starts: Vec<f64> = sorted.iter().map(|m| (m.bar.max(1) - 1) as f64 * bar_beats + m.beat).collect();
        let spans = sorted
            .iter()
            .enumerate()
            .map(|(i, m)| ChordSpan {
                start: starts[i],
                end: starts.get(i + 1).copied().unwrap_or(song_end.max(starts[i])),
                bar: m.bar,
                beat: m.beat,
                text: m.chord.clone(),
                parsed: ChordSymbol::parse(&m.chord),
            })
            .collect();
        ChordTrack { spans, bar_beats }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The span sounding at `beat` (None before the first mark and after the song end).
    pub fn at(&self, beat: f64) -> Option<&ChordSpan> {
        let i = self.spans.partition_point(|s| s.start <= beat + 1e-9);
        let s = self.spans.get(i.checked_sub(1)?)?;
        (beat < s.end - 1e-9).then_some(s)
    }

    /// The chord sounding at `beat` (None for no harmony, "N.C." or an unreadable symbol).
    pub fn chord_at(&self, beat: f64) -> Option<&Chord> {
        self.at(beat).and_then(ChordSpan::chord)
    }

    /// The spans overlapping `[from, to)`, clipped to it.
    pub fn spans_in(&self, from: f64, to: f64) -> Vec<ChordSpan> {
        self.spans
            .iter()
            .filter(|s| s.end > from + 1e-9 && s.start < to - 1e-9)
            .map(|s| ChordSpan { start: s.start.max(from), end: s.end.min(to), ..s.clone() })
            .collect()
    }

    /// Marks whose symbol could not be read: (bar, beat, error).
    pub fn errors(&self) -> Vec<(u32, f64, &ChordError)> {
        self.spans.iter().filter_map(|s| s.parsed.as_ref().err().map(|e| (s.bar, s.beat, e))).collect()
    }

    /// The chords for `[from, to)`: an error for an unreadable symbol or a stretch without harmony, so
    /// composing tools never voice a guess.
    pub fn require(&self, from: f64, to: f64) -> Result<Vec<ChordSpan>> {
        let spans = self.spans_in(from, to);
        let bar_of = |b: f64| (b / self.bar_beats.max(1e-9)).floor() as u32 + 1;
        if spans.first().is_none_or(|s| s.start > from + 1e-9) {
            return Err(Error::invalid(format!(
                "the chord track has no chord at bar {}: set one with the harmony action (or pass a chord chart)",
                bar_of(from)
            )));
        }
        for s in &spans {
            if let Err(e) = &s.parsed {
                return Err(Error::invalid(format!("the chord track at bar {} beat {}: {e}", s.bar, crate::util::fmt_num(s.beat))));
            }
        }
        Ok(spans)
    }
}

/// One chord of a chart, before it is placed on the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartChord {
    /// 0-based bar of the chart.
    pub bar: usize,
    /// Beat within the bar.
    pub beat: f64,
    pub symbol: ChordSymbol,
    pub text: String,
}

fn chart_err(position: usize, token: &str, message: impl Into<String>) -> TextError {
    TextError { what: "chord chart", position, token: token.to_string(), message: message.into() }
}

/// Splits on whitespace that is not inside parentheses; returns (char position, token).
fn chart_tokens(cell: &str, offset: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut start = 0;
    for (i, c) in cell.chars().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if c.is_whitespace() && depth <= 0 {
            if !cur.is_empty() {
                out.push((offset + start, std::mem::take(&mut cur)));
            }
        } else {
            if cur.is_empty() {
                start = i;
            }
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push((offset + start, cur));
    }
    out
}

/// Parses a chord chart (see the module docs) for bars of `bar_beats` beats.
pub fn parse_chart(text: &str, bar_beats: f64) -> Result<Vec<ChartChord>, TextError> {
    let mut out: Vec<ChartChord> = Vec::new();
    let mut offset = 0;
    let cells: Vec<&str> = text.split('|').collect();
    let n_cells = cells.len();
    let mut prev_bar: Vec<ChartChord> = Vec::new();
    for (bi, cell) in cells.into_iter().enumerate() {
        let cell_offset = offset;
        offset += cell.chars().count() + 1;
        let toks = chart_tokens(cell, cell_offset);
        if toks.is_empty() {
            if bi == n_cells - 1 && bi > 0 {
                break; // trailing barline
            }
            return Err(chart_err(cell_offset, cell, format!("bar {} is empty (write % to repeat the previous bar)", bi + 1)));
        }
        if toks.len() == 1 && toks[0].1 == "%" {
            if prev_bar.is_empty() {
                return Err(chart_err(toks[0].0, "%", "% repeats the previous bar, but there is none"));
            }
            let repeated: Vec<ChartChord> = prev_bar.iter().map(|c| ChartChord { bar: bi, ..c.clone() }).collect();
            out.extend(repeated.iter().cloned());
            prev_bar = repeated;
            continue;
        }
        let n = toks.len();
        let mut bar: Vec<ChartChord> = Vec::with_capacity(n);
        for (k, (pos, tok)) in toks.iter().enumerate() {
            let (sym, beat) = match tok.rsplit_once('@') {
                Some((s, b)) => {
                    let beat: f64 = b
                        .parse()
                        .ok()
                        .filter(|x: &f64| x.is_finite())
                        .ok_or_else(|| chart_err(*pos, tok, format!("bad beat {b:?} after @ (a number of beats into the bar)")))?;
                    (s, beat)
                }
                None => (tok.as_str(), k as f64 * bar_beats / n as f64),
            };
            if !(0.0..bar_beats - 1e-9).contains(&beat) {
                return Err(chart_err(*pos, tok, format!("beat {} is outside the bar (0 to {})", crate::util::fmt_num(beat), crate::util::fmt_num(bar_beats))));
            }
            if let Some(prev) = bar.last()
                && beat <= prev.beat + 1e-9
            {
                return Err(chart_err(*pos, tok, "chords in a bar must be in time order"));
            }
            if sym == "%" {
                return Err(chart_err(*pos, tok, "% stands alone in its bar"));
            }
            let symbol = ChordSymbol::parse(sym).map_err(|e| chart_err(*pos + e.position, tok, e.message.clone()))?;
            bar.push(ChartChord { bar: bi, beat, symbol, text: sym.trim().to_string() });
        }
        out.extend(bar.iter().cloned());
        prev_bar = bar;
    }
    Ok(out)
}

/// The number of bars in a chart (a trailing barline does not start a bar).
pub fn chart_bars(text: &str) -> usize {
    crate::notation::bar_count(text)
}

/// Chart chords as chord-track marks from `first_bar`, merging a chord that repeats the one before it
/// (a mark holds until the next, so repeats add nothing).
pub fn chart_marks(chart: &[ChartChord], first_bar: u32) -> Vec<ChordMark> {
    let mut out: Vec<ChordMark> = Vec::new();
    for c in chart {
        if out.last().is_some_and(|m: &ChordMark| m.chord == c.text) {
            continue;
        }
        out.push(ChordMark { bar: first_bar + c.bar as u32, beat: c.beat, chord: c.text.clone() });
    }
    out
}

/// The chord track with bars `[from_bar, to_bar]` replaced by `new` marks (which must lie in that range).
/// The chord that was sounding when the range ends is restored after it, so the rest of the song keeps
/// its harmony.
pub fn replace_marks(existing: &[ChordMark], new: &[ChordMark], from_bar: u32, to_bar: u32, song_bars: u32) -> Vec<ChordMark> {
    let mut sorted: Vec<ChordMark> = existing.to_vec();
    sorted.sort_by(|a, b| (a.bar, a.beat).partial_cmp(&(b.bar, b.beat)).unwrap_or(std::cmp::Ordering::Equal));
    let after = to_bar + 1;
    let sounding_after = sorted.iter().rfind(|m| m.bar < after).cloned();
    let has_mark_after = sorted.iter().any(|m| m.bar == after && m.beat == 0.0);
    let mut out: Vec<ChordMark> = sorted.iter().filter(|m| m.bar < from_bar).cloned().collect();
    for m in new {
        if out.last().is_some_and(|l| l.chord == m.chord) {
            continue;
        }
        out.push(m.clone());
    }
    if after <= song_bars
        && !has_mark_after
        && let Some(m) = sounding_after
        && out.last().is_none_or(|l| l.chord != m.chord)
    {
        out.push(ChordMark { bar: after, beat: 0.0, chord: m.chord });
    }
    out.extend(sorted.iter().filter(|m| m.bar > to_bar).cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(bar: u32, beat: f64, chord: &str) -> ChordMark {
        ChordMark { bar, beat, chord: chord.into() }
    }

    #[test]
    fn spans_and_lookup() {
        let t = ChordTrack::from_marks(&[mark(3, 0.0, "F"), mark(1, 0.0, "C"), mark(3, 2.0, "G7"), mark(5, 0.0, "N.C."), mark(6, 0.0, "Xq")], 4.0, 32.0);
        assert_eq!(t.spans.len(), 5);
        assert_eq!((t.spans[0].start, t.spans[0].end), (0.0, 8.0));
        assert_eq!(t.chord_at(9.99).unwrap().symbol, "F");
        assert_eq!(t.chord_at(10.0).unwrap().symbol, "G7");
        assert!(t.chord_at(16.5).is_none() && t.at(16.5).unwrap().is_no_chord());
        assert!(t.chord_at(32.0).is_none());
        assert_eq!(t.errors().len(), 1);
        let s = t.spans_in(6.0, 12.0);
        assert_eq!(s.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(), [(6.0, 8.0), (8.0, 10.0), (10.0, 12.0)]);
        assert!(t.require(0.0, 16.0).is_ok());
        assert!(t.require(20.0, 24.0).unwrap_err().to_string().contains("bar 6"));
        let late = ChordTrack::from_marks(&[mark(2, 0.0, "C")], 4.0, 16.0);
        assert!(late.require(0.0, 8.0).unwrap_err().to_string().contains("no chord at bar 1"));
        assert!(ChordTrack::default().at(0.0).is_none());
    }

    #[test]
    fn charts() {
        let c = parse_chart("Am | F C | G@0 G/B@2.5 | N.C. | % |", 4.0).unwrap();
        let v: Vec<(usize, f64, &str)> = c.iter().map(|c| (c.bar, c.beat, c.text.as_str())).collect();
        assert_eq!(v, [(0, 0.0, "Am"), (1, 0.0, "F"), (1, 2.0, "C"), (2, 0.0, "G"), (2, 2.5, "G/B"), (3, 0.0, "N.C."), (4, 0.0, "N.C.")]);
        assert_eq!(chart_bars("Am | F C | G |"), 3);
        let c = parse_chart("C7(b9 #11) Fmaj7", 4.0).unwrap();
        assert_eq!(c[0].text, "C7(b9 #11)");
        assert_eq!(c[1].beat, 2.0);
        // 6/8: three chords split a 3-beat bar into beats
        assert_eq!(parse_chart("A B C", 3.0).unwrap().iter().map(|c| c.beat).collect::<Vec<_>>(), [0.0, 1.0, 2.0]);
    }

    #[test]
    fn chart_errors() {
        let e = parse_chart("Am | Hm", 4.0).unwrap_err();
        assert_eq!((e.position, e.token.as_str()), (5, "Hm"));
        assert!(parse_chart("Am || C", 4.0).unwrap_err().message.contains("bar 2 is empty"));
        assert!(parse_chart("% | C", 4.0).unwrap_err().message.contains("none"));
        assert!(parse_chart("C@3 F@1", 4.0).unwrap_err().message.contains("time order"));
        assert!(parse_chart("C@4", 4.0).unwrap_err().message.contains("outside the bar"));
        assert!(parse_chart("C@x", 4.0).unwrap_err().message.contains("bad beat"));
        let e = parse_chart("Cmaj7 Dxyz", 4.0).unwrap_err();
        assert_eq!(e.position, 7);
    }

    #[test]
    fn marks_and_replacement() {
        let chart = parse_chart("Am | Am | F G", 4.0).unwrap();
        let marks = chart_marks(&chart, 5);
        assert_eq!(marks, [mark(5, 0.0, "Am"), mark(7, 0.0, "F"), mark(7, 2.0, "G")]);
        let existing = [mark(1, 0.0, "C"), mark(9, 0.0, "E"), mark(6, 0.0, "D")];
        let out = replace_marks(&existing, &marks, 5, 7, 16);
        assert_eq!(out, [mark(1, 0.0, "C"), mark(5, 0.0, "Am"), mark(7, 0.0, "F"), mark(7, 2.0, "G"), mark(8, 0.0, "D"), mark(9, 0.0, "E")]);
        // a mark already at the bar after the range needs no restore
        let out = replace_marks(&[mark(1, 0.0, "C"), mark(8, 0.0, "E")], &marks, 5, 7, 16);
        assert_eq!(out.len(), 5);
        // at the song end nothing is restored
        let out = replace_marks(&[mark(1, 0.0, "C")], &marks, 5, 7, 7);
        assert_eq!(out.last().unwrap(), &mark(7, 2.0, "G"));
        // a new range starting with the chord already sounding merges into it
        let out = replace_marks(&[mark(1, 0.0, "Am")], &[mark(3, 0.0, "Am"), mark(4, 0.0, "C")], 3, 4, 8);
        assert_eq!(out, [mark(1, 0.0, "Am"), mark(4, 0.0, "C"), mark(5, 0.0, "Am")]);
    }
}
