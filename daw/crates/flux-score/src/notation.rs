//! Melody notation: a line written as text, bar by bar.
//!
//! ```text
//! D5:3 G5:3 | F5:2 E5:1 D5:2 C5:1 | r:2 A4:1 ~:3
//! ```
//!
//! * `pitch:units` is a note: a name with octave (`C4` = 60, `F#3`, `Bb5`) or a MIDI number, and its length
//!   in grid units (the `unit`, in beats: 0.5 = eighth notes). Lengths may be decimals (`1.5`) or fractions
//!   (`1/3`, for triplets).
//! * `r:units` is a rest.
//! * `~:units` ties more length onto the note before it (across a barline too).
//! * `|` is a barline. Every bar must add up to exactly one bar of units (a 6/8 bar in eighths is 6), so a
//!   miscounted bar is caught where it happens. A trailing `|` is allowed.
//!
//! Notes run into the next note (`legato`, a fraction of their length) and are shortened before rests and at
//! the end of the line (`short`); `arc` adds velocity towards the middle of the line. This is score.py's
//! `melody()`, with the same rounding, plus ties, fractions and precise errors.

use crate::error::{Error, Result, TextError};
use crate::theory::note::parse_note;
use crate::util::{beat4, velocity};
use flux_core::Note;

/// One token of a melody: a note (`pitch` set) or a rest, with ties already merged into the note.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// MIDI pitch, or None for a rest.
    pub pitch: Option<u8>,
    /// Length in grid units.
    pub units: f64,
    /// 0-based bar (within the text) the token starts in.
    pub bar: usize,
    /// Character position of the token in the text (0-based).
    pub position: usize,
}

/// How a melody is laid out.
#[derive(Clone, Debug, PartialEq)]
pub struct MelodyOptions {
    /// Beats per grid unit (1 = quarter notes, 0.5 = eighths).
    pub unit: f64,
    /// Base velocity.
    pub vel: f64,
    /// Semitones added to every pitch.
    pub transpose: i32,
    /// Fraction of its length a note sounds when another note follows.
    pub legato: f64,
    /// Fraction of its length a note sounds before a rest and at the end.
    pub short: f64,
    /// Velocity added towards the middle of the line (0 at the ends).
    pub arc: f64,
}

impl Default for MelodyOptions {
    fn default() -> MelodyOptions {
        MelodyOptions { unit: 1.0, vel: 90.0, transpose: 0, legato: 1.0, short: 0.85, arc: 0.0 }
    }
}

fn text_err(position: usize, token: &str, message: impl Into<String>) -> TextError {
    TextError { what: "notation", position, token: token.to_string(), message: message.into() }
}

/// Parses a length: "3", "1.5", "1/3".
fn parse_units(s: &str) -> Option<f64> {
    let v = match s.split_once('/') {
        Some((a, b)) => {
            let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            if b == 0.0 {
                return None;
            }
            a / b
        }
        None => s.trim().parse().ok()?,
    };
    (v.is_finite() && v > 0.0).then_some(v)
}

/// Splits notation into tokens, checking that every bar holds `units_per_bar` units (within 1e-6).
pub fn parse_notation(text: &str, units_per_bar: f64) -> Result<Vec<Token>, TextError> {
    let mut out: Vec<Token> = Vec::new();
    // byte offset -> char position, for messages
    let char_pos = |byte: usize| text[..byte].chars().count();
    let mut bar_start = 0usize;
    let bars: Vec<(usize, &str)> = text
        .split('|')
        .map(|b| {
            let s = bar_start;
            bar_start += b.len() + 1;
            (s, b)
        })
        .collect();
    let n_bars = bars.len();
    for (bi, (offset, bar)) in bars.into_iter().enumerate() {
        if bar.trim().is_empty() {
            if bi == n_bars - 1 && bi > 0 {
                break; // trailing barline
            }
            return Err(text_err(char_pos(offset), bar, format!("bar {} is empty", bi + 1)));
        }
        let mut total = 0.0;
        let mut last_pos = 0;
        let mut search = 0usize;
        for tok in bar.split_whitespace() {
            let local = search + bar[search..].find(tok).unwrap_or(0);
            search = local + tok.len();
            let pos = char_pos(offset + local);
            last_pos = pos;
            let (p, d) = tok
                .split_once(':')
                .ok_or_else(|| text_err(pos, tok, "expected pitch:units (e.g. D5:2), r:units for a rest or ~:units for a tie"))?;
            let units = parse_units(d).ok_or_else(|| text_err(pos, tok, format!("bad length {d:?} (a positive number of units, e.g. 2, 1.5 or 1/3)")))?;
            total += units;
            match p {
                "r" | "R" => out.push(Token { pitch: None, units, bar: bi, position: pos }),
                "~" => match out.last_mut() {
                    Some(prev) if prev.pitch.is_some() => prev.units += units,
                    _ => return Err(text_err(pos, tok, "a tie (~) must follow a note")),
                },
                _ => {
                    let pitch = parse_note(p).map_err(|e| text_err(pos, tok, e.message))?;
                    out.push(Token { pitch: Some(pitch), units, bar: bi, position: pos });
                }
            }
        }
        if (total - units_per_bar).abs() > 1e-6 {
            let bar_text = bar.trim();
            return Err(text_err(
                last_pos,
                bar_text,
                format!("bar {} adds up to {} units, but a bar holds {}", bi + 1, crate::util::fmt_num(total), crate::util::fmt_num(units_per_bar)),
            ));
        }
    }
    Ok(out)
}

/// Lays a melody out from `start` (an absolute beat, normally a downbeat). `bar_beats` is the bar length
/// in beats. Notes are rounded like score.py (`beat` and `dur` to 4 decimals, velocity to an integer
/// 1-127, ties to even).
pub fn melody(text: &str, start: f64, bar_beats: f64, opts: &MelodyOptions) -> Result<Vec<Note>> {
    if !(opts.unit.is_finite() && opts.unit > 0.0) {
        return Err(Error::invalid(format!("unit must be a positive number of beats, got {}", opts.unit)));
    }
    let toks = parse_notation(text, bar_beats / opts.unit)?;
    let mut t = start;
    let mut out = Vec::with_capacity(toks.len());
    let last = toks.len().saturating_sub(1).max(1) as f64;
    for (i, tok) in toks.iter().enumerate() {
        if let Some(p) = tok.pitch {
            let next_is_note = toks.get(i + 1).is_some_and(|n| n.pitch.is_some());
            let len = tok.units * opts.unit * if next_is_note { opts.legato } else { opts.short };
            let pos = i as f64 / last;
            let pitch = p as i32 + opts.transpose;
            if !(0..=127).contains(&pitch) {
                return Err(text_err(tok.position, &crate::theory::note::note_name(p), format!("transposed by {:+} it leaves the MIDI range", opts.transpose)).into());
            }
            out.push(Note {
                start: beat4(t),
                dur: beat4(len.max(0.02)),
                pitch: pitch as u8,
                vel: velocity(opts.vel + opts.arc * (1.0 - (2.0 * pos - 1.0).abs())),
            });
        }
        t += tok.units * opts.unit;
    }
    Ok(out)
}

/// Bars `[first, first + count)` (1-based) of a notation string, like score.py's `bars_of`.
pub fn bars_of(text: &str, first: usize, count: usize) -> String {
    let parts: Vec<&str> = text.split('|').collect();
    let a = first.saturating_sub(1).min(parts.len());
    let b = (a + count).min(parts.len());
    parts[a..b].join("|")
}

/// The number of bars in a notation string (a trailing barline does not start a bar).
pub fn bar_count(text: &str) -> usize {
    let parts: Vec<&str> = text.split('|').collect();
    let n = parts.len();
    if n > 1 && parts[n - 1].trim().is_empty() { n - 1 } else { n }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(start: f64, dur: f64, pitch: u8, vel: u8) -> Note {
        Note { start, dur, pitch, vel }
    }

    #[test]
    fn lays_out_like_score_py() {
        // 6/8 in eighths, from bar 2 (beat 3)
        let o = MelodyOptions { unit: 0.5, vel: 80.0, ..Default::default() };
        let notes = melody("D5:3 G5:3 | F5:2 E5:1 D5:2 r:1", 3.0, 3.0, &o).unwrap();
        assert_eq!(
            notes,
            [n(3.0, 1.5, 74, 80), n(4.5, 1.5, 79, 80), n(6.0, 1.0, 77, 80), n(7.0, 0.5, 76, 80), n(7.5, 0.85, 74, 80)]
        );
    }

    #[test]
    fn arc_and_transpose() {
        let o = MelodyOptions { arc: 10.0, transpose: -12, ..Default::default() };
        let notes = melody("C5:1 D5:1 E5:1 F5:1", 0.0, 4.0, &o).unwrap();
        assert_eq!(notes.iter().map(|x| x.vel).collect::<Vec<_>>(), [90, 97, 97, 90]);
        assert_eq!(notes[0].pitch, 60);
        assert_eq!(notes[3].dur, 0.85);
    }

    #[test]
    fn ties_fractions_and_trailing_barline() {
        let notes = melody("C4:3 ~:1 | ~:2 r:2 |", 0.0, 4.0, &MelodyOptions::default()).unwrap();
        assert_eq!(notes, [n(0.0, 5.1, 60, 90)]);
        let notes = melody("C4:2/3 D4:2/3 E4:2/3 F4:2", 0.0, 4.0, &MelodyOptions::default()).unwrap();
        assert_eq!(notes.iter().map(|x| x.start).collect::<Vec<_>>(), [0.0, 0.6667, 1.3333, 2.0]);
    }

    #[test]
    fn errors_name_the_token_and_bar() {
        let e = parse_notation("C4:2 D4:2 | E4:2 F4:1", 4.0).unwrap_err();
        assert!(e.message.contains("bar 2 adds up to 3 units"), "{e}");
        let e = parse_notation("C4:2 H4:2", 4.0).unwrap_err();
        assert_eq!((e.position, e.token.as_str()), (5, "H4:2"));
        let e = parse_notation("C4:2 D4", 4.0).unwrap_err();
        assert!(e.message.contains("pitch:units"));
        let e = parse_notation("C4:0 D4:4", 4.0).unwrap_err();
        assert!(e.message.contains("bad length"));
        assert!(parse_notation("~:2 C4:2", 4.0).unwrap_err().message.contains("tie"));
        assert!(parse_notation("C4:4 || D4:4", 4.0).unwrap_err().message.contains("bar 2 is empty"));
        let e = melody("C9:4", 0.0, 4.0, &MelodyOptions { transpose: 12, ..Default::default() }).unwrap_err();
        assert!(e.to_string().contains("MIDI range"), "{e}");
        assert!(melody("C4:4", 0.0, 4.0, &MelodyOptions { unit: 0.0, ..Default::default() }).is_err());
    }

    #[test]
    fn bar_helpers() {
        assert_eq!(bars_of("a | b | c | d", 2, 2), " b | c ");
        assert_eq!(bars_of("a | b", 3, 1), "");
        assert_eq!(bar_count("a | b |"), 2);
        assert_eq!(bar_count("a"), 1);
    }
}
