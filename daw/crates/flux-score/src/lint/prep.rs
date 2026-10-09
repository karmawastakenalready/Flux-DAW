//! Tracks as the linter hears them: keyswitch blips set apart, pitch bends and transpositions applied, and
//! the line helpers of tools/score_lint.py (`top_line`, `passing`).

use super::catalog::InstrumentInfo;
use super::{MIN_DUR, TOL};
use crate::util::{py_round, round_even};
use flux_core::{Note, Track};
use std::collections::HashMap;

/// A note as heard: `pitch` includes the track's transposition and the pitch bend; `key` is as written.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Heard {
    pub start: f64,
    pub dur: f64,
    pub pitch: i32,
    pub vel: u8,
    pub key: u8,
    /// The track's index in the project.
    pub track: usize,
}

impl Heard {
    pub fn end(&self) -> f64 {
        self.start + self.dur
    }
}

/// A track ready for the checks.
pub(crate) struct LTrack<'a> {
    pub track: &'a Track,
    pub pitched: bool,
    /// Notes of at least [`MIN_DUR`] beats, sorted like score_lint.py (start, length, pitch, velocity).
    pub notes: Vec<Heard>,
    /// Shorter notes: keyswitches (or strays).
    pub blips: Vec<Note>,
    /// Longer notes on keys the instrument's card lists as keyswitches (not heard; only the keyswitch
    /// check looks at them).
    pub held_switches: Vec<Heard>,
    pub bent: bool,
    /// The instrument's card (or the caller's range for it).
    pub info: Option<InstrumentInfo>,
}

impl LTrack<'_> {
    pub fn name(&self) -> &str {
        &self.track.name
    }
}

/// score_lint.py's UNPITCHED pattern: `drum|kick|snare|\btoms?\b|cymbal|hi-?hat|perc|clock|industrial|noise|\bfx\b`
/// (case-insensitive).
pub(crate) fn unpitched_name(text: &str) -> bool {
    let s = text.to_lowercase();
    if ["drum", "kick", "snare", "cymbal", "hihat", "hi-hat", "perc", "clock", "industrial", "noise"].iter().any(|w| s.contains(w)) {
        return true;
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    ["toms", "tom", "fx"].iter().any(|w| {
        s.match_indices(w).any(|(i, _)| !word(s[..i].chars().next_back()) && !word(s[i + w.len()..].chars().next()))
    })
}

/// A track's notes as heard (score_lint.py's `load`): the bend settled a little into each note
/// (`min(0.3, dur / 2)` beats) shifts it by `round(value / 8192 * bend_range)` semitones.
pub(crate) fn heard_notes(t: &Track, track: usize, shift: i32, bend_range: f64) -> Vec<Heard> {
    let mut bends = t.bends.clone();
    bends.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    let mut out: Vec<Heard> = t
        .notes
        .iter()
        .filter(|n| n.dur >= MIN_DUR)
        .map(|n| {
            let at = n.start + (n.dur / 2.0).min(0.3) + 1e-9;
            let i = bends.partition_point(|b| b.beat <= at);
            let bend = if i > 0 { round_even(bends[i - 1].value as f64 / 8192.0 * bend_range) as i32 } else { 0 };
            Heard { start: n.start, dur: n.dur, pitch: n.pitch as i32 + shift + bend, vel: n.vel, key: n.pitch, track }
        })
        .collect();
    out.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.dur.total_cmp(&b.dur)).then(a.pitch.cmp(&b.pitch)).then(a.vel.cmp(&b.vel)));
    out
}

/// A track's melody (score_lint.py's `top_line`): the highest note of each onset (within [`TOL`]), skipping
/// notes that start more than an octave under a line note still sounding (the accompaniment under a held
/// melody note).
pub(crate) fn top_line(notes: &[Heard]) -> Vec<Heard> {
    let mut line: Vec<Heard> = Vec::new();
    for n in notes {
        match line.last_mut() {
            Some(last) if n.start - last.start < TOL => {
                if n.pitch > last.pitch {
                    *last = Heard { start: last.start, ..*n };
                }
            }
            Some(last) if last.start + last.dur > n.start + 0.25 && n.pitch < last.pitch - 12 => {}
            _ => line.push(*n),
        }
    }
    line
}

/// Which notes are short passing notes (score_lint.py's `passing`): under a beat and a step (1-2 semitones)
/// from a note at a neighbouring onset that is close in time.
pub(crate) fn passing(notes: &[Heard]) -> Vec<bool> {
    let key = |s: f64| py_round(s, 2).to_bits();
    let mut onsets: Vec<f64> = notes.iter().map(|n| py_round(n.start, 2)).collect();
    onsets.sort_by(f64::total_cmp);
    onsets.dedup();
    let mut at: HashMap<u64, Vec<i32>> = HashMap::new();
    for n in notes {
        at.entry(key(n.start)).or_default().push(n.pitch);
    }
    let index: HashMap<u64, usize> = onsets.iter().enumerate().map(|(i, o)| (o.to_bits(), i)).collect();
    notes
        .iter()
        .map(|n| {
            if n.dur >= 1.0 {
                return false;
            }
            let i = index[&key(n.start)];
            let near = [i.checked_sub(1), Some(i + 1)]
                .into_iter()
                .flatten()
                .filter(|&j| j < onsets.len() && (onsets[j] - n.start).abs() <= n.dur.max(0.5) + 0.25)
                .flat_map(|j| at[&onsets[j].to_bits()].iter());
            near.into_iter().any(|q| (1..=2).contains(&(n.pitch - q).abs()))
        })
        .collect()
}

/// The median of a track's top line (score_lint.py's melody guess: a top line mostly at C4 or above).
pub(crate) fn top_median(notes: &[Heard]) -> Option<i32> {
    let mut ps: Vec<i32> = top_line(notes).iter().map(|n| n.pitch).collect();
    ps.sort_unstable();
    ps.get(ps.len() / 2).copied()
}

/// Whether a track plays one note at a time (at least 90% of its notes overlap no other by more than
/// [`TOL`]) and has enough notes to call it a line.
pub(crate) fn is_line(notes: &[Heard]) -> bool {
    if notes.len() < 4 {
        return false;
    }
    let mut overlapping = 0;
    for (i, n) in notes.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| notes[j]);
        let next = notes.get(i + 1);
        if prev.is_some_and(|p| p.end() > n.start + TOL) || next.is_some_and(|x| n.end() > x.start + TOL) {
            overlapping += 1;
        }
    }
    overlapping * 10 <= notes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::BendPoint;

    fn h(start: f64, dur: f64, pitch: i32) -> Heard {
        Heard { start, dur, pitch, vel: 80, key: pitch.clamp(0, 127) as u8, track: 0 }
    }

    #[test]
    fn unpitched_names() {
        for s in ["Drums", "Kick", "Floor Tom", "toms", "Hi-Hat", "hihat", "Perc", "FX", "sfz/fx.sfz", "Noise Riser"] {
            assert!(unpitched_name(s), "{s}");
        }
        // `` treats `_` as part of a word, as Python's re does
        for s in ["Tomato", "Atom", "Flute", "fxbus", "Effects", "sfz/makima_fx.sfz"] {
            assert!(!unpitched_name(s), "{s}");
        }
    }

    #[test]
    fn bends_and_transposition() {
        let mut t: Track = serde_json::from_value(serde_json::json!({"id": 1, "name": "Lead", "color": [0, 0, 0],
            "instrument": {"kind": "none"}}))
        .unwrap_or_else(|e| panic!("{e}"));
        t.notes = vec![Note { start: 0.0, dur: 1.0, pitch: 60, vel: 90 }, Note { start: 2.0, dur: 0.02, pitch: 24, vel: 90 }, Note { start: 4.0, dur: 1.0, pitch: 60, vel: 90 }];
        t.bends = vec![BendPoint { beat: 4.2, value: 4096 }, BendPoint { beat: 0.5, value: 8191 }];
        let n = heard_notes(&t, 3, 12, 2.0);
        assert_eq!(n.iter().map(|n| (n.pitch, n.key, n.track)).collect::<Vec<_>>(), [(72, 60, 3), (73, 60, 3)]);
    }

    #[test]
    fn lines_and_passing_notes() {
        // a held melody note with chords well under it: the line keeps the melody
        let notes = [h(0.0, 2.0, 72), h(0.0, 1.0, 55), h(0.5, 0.5, 52), h(2.0, 1.0, 74), h(2.0, 1.0, 60)];
        let line = top_line(&notes);
        assert_eq!(line.iter().map(|n| n.pitch).collect::<Vec<_>>(), [72, 74]);
        let run = [h(0.0, 0.5, 60), h(0.5, 0.5, 62), h(1.0, 1.0, 64), h(3.0, 0.5, 70)];
        assert_eq!(passing(&run), [true, true, false, false]);
        assert_eq!(top_median(&run), Some(64));
        assert!(is_line(&run));
        assert!(!is_line(&[h(0.0, 1.0, 60), h(0.0, 1.0, 64), h(1.0, 1.0, 62), h(1.0, 1.0, 65)]));
    }
}
