//! Checks score_lint.py does not have: part writing (parallels, crossings, leaps) and data hygiene
//! (self-overlaps, controller pile-ups, data past the end, keyswitches, unreadable chord marks).

use super::checks::key;
use super::prep::{Heard, LTrack, is_line, top_line, top_median};
use super::{Acc, Draft, Kind, Severity, TOL, pname};
use crate::harmony::ChordTrack;
use crate::theory::note::note_name;
use crate::util::fmt_num;
use flux_core::{Project, TimeMap};
use std::collections::BTreeMap;

/// Beats that count as "at the same time" for controller points and duplicate notes.
const SAME: f64 = 1e-3;

fn by_time(a: &Heard, b: &Heard) -> std::cmp::Ordering {
    a.start.total_cmp(&b.start).then(a.dur.total_cmp(&b.dur)).then(a.pitch.cmp(&b.pitch)).then(a.vel.cmp(&b.vel)).then(a.track.cmp(&b.track))
}

/// Interval names up to two octaves.
fn interval_name(semitones: i32) -> String {
    const NAMES: [&str; 25] = [
        "unison", "minor 2nd", "major 2nd", "minor 3rd", "major 3rd", "perfect 4th", "tritone", "perfect 5th", "minor 6th", "major 6th", "minor 7th", "major 7th", "octave",
        "minor 9th", "major 9th", "minor 10th", "major 10th", "perfect 11th", "augmented 11th", "perfect 12th", "minor 13th", "major 13th", "minor 14th", "major 14th",
        "two octaves",
    ];
    let n = semitones.unsigned_abs() as usize;
    match NAMES.get(n) {
        Some(name) => format!("{n} semitones, {} {name}", if name.starts_with(['a', 'o']) { "an" } else { "a" }),
        None => format!("{n} semitones"),
    }
}

/// Parallel fifths and octaves between the melody (the top line of the melody tracks together) and the
/// bass (the lowest pitched note sounding): both move the same way at the same onset, onto new notes, and
/// keep a fifth or an octave (compound or not) between them. Notes of one track against itself are skipped.
pub(crate) fn parallels(tracks: &[LTrack], melody: &[usize], acc: &mut Acc) {
    let mut sop: Vec<Heard> = melody.iter().filter(|&&i| tracks[i].pitched).flat_map(|&i| tracks[i].notes.iter().copied()).collect();
    sop.sort_by(by_time);
    let sop = top_line(&sop);
    let mut all: Vec<Heard> = tracks.iter().filter(|t| t.pitched).flat_map(|t| t.notes.iter().copied()).collect();
    all.sort_by(by_time);
    let longest = all.iter().map(|n| n.dur).fold(0.0, f64::max);
    // the lowest note sounding just after `t`
    let bass_at = |t: f64| -> Option<Heard> {
        let first = all.partition_point(|n| n.start < t + TOL - longest - 1e-9);
        let last = all.partition_point(|n| n.start <= t + TOL);
        all[first..last].iter().filter(|n| n.end() > t + TOL).min_by_key(|n| (n.pitch, n.track)).copied()
    };
    for w in sop.windows(2) {
        let (s0, s1) = (w[0], w[1]);
        if s1.pitch == s0.pitch || s1.start - s0.end() >= 1.0 {
            continue;
        }
        let (Some(b0), Some(b1)) = (bass_at(s0.start), bass_at(s1.start)) else { continue };
        if b1.pitch == b0.pitch || b1.start < s1.start - TOL || b0.track == s0.track || b1.track == s1.track {
            continue;
        }
        let (d0, d1) = (s0.pitch - b0.pitch, s1.pitch - b1.pitch);
        if d0 < 7 || d1 < 7 || (s1.pitch - s0.pitch).signum() != (b1.pitch - b0.pitch).signum() || d0 % 12 != d1 % 12 {
            continue;
        }
        let (kind, what) = match d1 % 12 {
            7 => (Kind::ParallelFifths, "fifths"),
            0 => (Kind::ParallelOctaves, "octaves"),
            _ => continue,
        };
        let (sop_t, bass_t) = (tracks[s1.track].name(), tracks[b1.track].name());
        let draft = Draft {
            kind,
            severity: Severity::Info,
            tracks: vec![sop_t.to_string(), bass_t.to_string()],
            message: format!(
                "{sop_t} and {bass_t} move in parallel {what}: {}/{} -> {}/{}",
                pname(s0.pitch),
                pname(b0.pitch),
                pname(s1.pitch),
                pname(b1.pitch)
            ),
            fix: format!("fine if {bass_t} doubles the melody on purpose; otherwise move the bass against the melody (contrary or oblique motion) or use another inversion"),
        };
        acc.add(s1.start, key(&[&sop_t, &bass_t]), draft);
    }
}

/// Tracks playing single lines: (track, median of its line).
fn lines(tracks: &[LTrack]) -> Vec<(usize, i32)> {
    tracks.iter().enumerate().filter(|(_, t)| t.pitched && is_line(&t.notes)).filter_map(|(i, t)| top_median(&t.notes).map(|m| (i, m))).collect()
}

/// A lower line sounding above a higher one (by their medians, at least 3 semitones apart) for half a beat
/// or more.
pub(crate) fn crossing(tracks: &[LTrack], acc: &mut Acc) {
    let lines = lines(tracks);
    for &(hi, hi_median) in &lines {
        for &(lo, lo_median) in &lines {
            if hi_median < lo_median + 3 {
                continue;
            }
            let (upper, lower) = (&tracks[hi], &tracks[lo]);
            let longest = lower.notes.iter().map(|n| n.dur).fold(0.0, f64::max);
            for u in &upper.notes {
                let first = lower.notes.partition_point(|n| n.start < u.start - longest - 1e-9);
                for n in lower.notes[first..].iter().take_while(|n| n.start < u.end()) {
                    let overlap = n.end().min(u.end()) - n.start.max(u.start);
                    if n.pitch > u.pitch && overlap >= 0.5 {
                        let draft = Draft {
                            kind: Kind::VoiceCrossing,
                            severity: Severity::Info,
                            tracks: vec![lower.name().to_string(), upper.name().to_string()],
                            message: format!("{} {} crosses above {} {}", lower.name(), pname(n.pitch), upper.name(), pname(u.pitch)),
                            fix: "fine if the crossing is meant; otherwise swap the two notes between the parts or move one an octave".into(),
                        };
                        acc.add(n.start.max(u.start), key(&[&lower.name(), &upper.name()]), draft);
                    }
                }
            }
        }
    }
}

/// Whether a line moves mostly by leaps (more than a fourth in at least half its moves): arpeggios and
/// bass figures, where wide leaps are the pattern rather than a problem.
fn is_figuration(line: &[Heard]) -> bool {
    let moves: Vec<i32> = line.windows(2).map(|w| (w[1].pitch - w[0].pitch).abs()).filter(|&m| m > 0).collect();
    !moves.is_empty() && moves.iter().filter(|&&m| m > 5).count() * 2 >= moves.len()
}

/// Leaps wider than `max_leap` semitones inside a phrase (less than a beat of rest between the notes), in
/// the melody tracks and the tracks that play single lines, unless the line is a figuration (see
/// [`is_figuration`]).
pub(crate) fn leaps(tracks: &[LTrack], melody: &[usize], max_leap: u8, acc: &mut Acc) {
    for (i, t) in tracks.iter().enumerate() {
        if !t.pitched || !(melody.contains(&i) || is_line(&t.notes)) {
            continue;
        }
        let line = top_line(&t.notes);
        if is_figuration(&line) {
            continue;
        }
        for w in line.windows(2) {
            let (a, b) = (w[0], w[1]);
            let leap = (b.pitch - a.pitch).abs();
            if leap <= max_leap as i32 || b.start - a.end() >= 1.0 - 1e-9 {
                continue;
            }
            let draft = Draft {
                kind: Kind::Leap,
                severity: Severity::Info,
                tracks: vec![t.name().to_string()],
                message: format!("{} leaps {} from {} to {}", t.name(), interval_name(leap), pname(a.pitch), pname(b.pitch)),
                fix: "fill the leap with a passing note, move one side an octave, or put a rest before it".into(),
            };
            acc.add(b.start, key(&[&t.name(), &a.pitch, &b.pitch]), draft);
        }
    }
}

/// The same key twice at once on one track: notes that start together (duplicates), and on pitched
/// tracks a note still sounding when the next one on its key starts.
pub(crate) fn overlaps(tracks: &[LTrack], acc: &mut Acc) {
    for t in tracks {
        let mut by_key: BTreeMap<u8, Vec<&Heard>> = BTreeMap::new();
        for n in &t.notes {
            by_key.entry(n.key).or_default().push(n);
        }
        for (k, ns) in by_key {
            let mut ns = ns;
            ns.sort_by(|a, b| a.start.total_cmp(&b.start));
            for w in ns.windows(2) {
                let (a, b) = (w[0], w[1]);
                let draft = if b.start - a.start < SAME {
                    let what = if (a.dur - b.dur).abs() < SAME && a.vel == b.vel { "an exact duplicate" } else { "two notes that start together" };
                    Draft {
                        kind: Kind::SelfOverlap,
                        severity: Severity::Error,
                        tracks: vec![t.name().to_string()],
                        message: format!("{} {} is {what}", t.name(), note_name(k)),
                        fix: "delete one of them (delete_notes with its bar, beat and pitch)".into(),
                    }
                } else if t.pitched && a.end() > b.start + TOL {
                    Draft {
                        kind: Kind::SelfOverlap,
                        severity: Severity::Warning,
                        tracks: vec![t.name().to_string()],
                        message: format!(
                            "{} {} still sounds when the next {} starts ({} beats of overlap; the first note's note-off can cut the repeat short)",
                            t.name(),
                            note_name(k),
                            note_name(k),
                            fmt_num(a.end() - b.start)
                        ),
                        fix: "shorten the first note to end where the next one starts".into(),
                    }
                } else {
                    continue;
                };
                let tag = if draft.severity == Severity::Error { "dup" } else { "ring" };
                acc.add(b.start, key(&[&t.name(), &tag, &k]), draft);
            }
        }
    }
}

/// Groups of points at the same beat (within [`SAME`]): (beat, values in the order they apply).
fn piles<T: Copy>(mut points: Vec<(f64, T)>) -> Vec<(f64, Vec<T>)> {
    // stable: points at one beat keep their order, which is the order the renderer applies them in
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, Vec<T>)> = Vec::new();
    for (beat, v) in points {
        match out.last_mut() {
            Some((b, vs)) if beat - *b < SAME => vs.push(v),
            _ => out.push((beat, vec![v])),
        }
    }
    out.retain(|(_, vs)| vs.len() > 1);
    out
}

/// Several points of one controller (a CC number, or the pitch bend) at the same beat: what repeated
/// curve writes leave behind. One finding per track and controller.
pub(crate) fn controllers(tracks: &[LTrack], acc: &mut Acc) {
    for t in tracks {
        let mut ccs: BTreeMap<u8, Vec<(f64, i32)>> = BTreeMap::new();
        for c in &t.track.ccs {
            ccs.entry(c.cc).or_default().push((c.beat, c.value as i32));
        }
        let mut controls: Vec<(String, Vec<(f64, i32)>)> = ccs.into_iter().map(|(cc, pts)| (format!("CC{cc}"), pts)).collect();
        controls.push(("pitch-bend".into(), t.track.bends.iter().map(|b| (b.beat, b.value as i32)).collect()));
        for (what, pts) in controls {
            let groups: Vec<(f64, Vec<i32>)> = piles(pts).into_iter().filter(|(b, _)| acc.in_range(*b)).collect();
            let Some(first) = groups.first().map(|g| g.0) else { continue };
            let differ = groups.iter().filter(|(_, vs)| vs.iter().any(|v| *v != vs[0])).count();
            let most = groups.iter().map(|(_, vs)| vs.len()).max().unwrap_or(0);
            let example = groups.iter().find(|(_, vs)| vs.iter().any(|v| *v != vs[0])).unwrap_or(&groups[0]);
            let values: Vec<String> = example.1.iter().map(i32::to_string).collect();
            let message = format!(
                "{}: {} {what} points at the same beat in {} place{} (up to {most} at once{}; e.g. bar {} beat {}: {})",
                t.name(),
                groups.iter().map(|(_, v)| v.len()).sum::<usize>(),
                groups.len(),
                if groups.len() == 1 { "" } else { "s" },
                if differ > 0 { format!(", {differ} with different values: the last one wins") } else { ", same values".into() },
                acc.bar(example.0),
                fmt_num((example.0 - (acc.bar(example.0) - 1) as f64 * acc.bar_len()).max(0.0)),
                values.join(", ")
            );
            let fix = format!("clear the {what} data over those bars and write the curve once (the compose curve and swell actions replace their span)");
            let severity = if differ > 0 { Severity::Warning } else { Severity::Info };
            let k = key(&[&t.name(), &what]);
            acc.add(first, k.clone(), Draft { kind: Kind::ControllerPileUp, severity, tracks: vec![t.name().to_string()], message, fix });
            for (beat, _) in &groups[1..] {
                acc.add(*beat, k.clone(), Draft { kind: Kind::ControllerPileUp, severity, tracks: vec![], message: String::new(), fix: String::new() });
            }
        }
    }
}

/// Notes that start after the end of the song (in a looped song they fold onto its start; otherwise they
/// only play in the release tail), notes cut off by the end of a non-looped render (song plus the master
/// tail), and controller data after the end.
pub(crate) fn past_end(tracks: &[LTrack], project: &Project, acc: &mut Acc) {
    let end = project.total_beats();
    let tm = TimeMap::new(project);
    let render_end = tm.sec(end) + project.master.tail_s;
    let fold = if project.looped { "it folds onto the start of the loop" } else { "it only plays in the release tail" };
    for t in tracks {
        for n in &t.track.notes {
            let draft = if n.start >= end - 1e-9 {
                Draft {
                    kind: Kind::PastEnd,
                    severity: Severity::Warning,
                    tracks: vec![t.name().to_string()],
                    message: format!("{} {} starts after the end of the song (bar {}): {fold}", t.name(), note_name(n.pitch), project.bars),
                    fix: "delete it, or lengthen the song (set_project bars)".into(),
                }
            } else if !project.looped && tm.sec(n.start + n.dur) > render_end + 1e-6 {
                Draft {
                    kind: Kind::PastEnd,
                    severity: Severity::Warning,
                    tracks: vec![t.name().to_string()],
                    message: format!(
                        "{} {} is cut off by the end of the render ({} s after the song ends; the tail is {} s)",
                        t.name(),
                        note_name(n.pitch),
                        fmt_num(tm.sec(n.start + n.dur) - tm.sec(end)),
                        fmt_num(project.master.tail_s)
                    ),
                    fix: "end it earlier, or lengthen the master tail (set_master tail_s)".into(),
                }
            } else {
                continue;
            };
            let tag = if n.start >= end - 1e-9 { "start" } else { "cut" };
            acc.add(n.start, key(&[&t.name(), &tag, &n.pitch]), draft);
        }
        let late: Vec<f64> = t.track.ccs.iter().map(|c| c.beat).chain(t.track.bends.iter().map(|b| b.beat)).filter(|&b| b >= end - 1e-9).collect();
        if let Some(first) = late.iter().copied().reduce(f64::min) {
            let draft = Draft {
                kind: Kind::PastEnd,
                severity: Severity::Info,
                tracks: vec![t.name().to_string()],
                message: format!("{}: {} controller point{} after the end of the song", t.name(), late.len(), if late.len() == 1 { "" } else { "s" }),
                fix: "clear them (clear_cc / clear_bars) unless they shape the tail".into(),
            };
            acc.add(first, key(&[&t.name(), &"controllers"]), draft);
        }
    }
}

/// Keyswitch hygiene, for tracks whose instrument card lists keyswitches: keyswitch keys held as notes
/// (they switch but do not sound) and short blips on keys that are not keyswitches (strays).
pub(crate) fn keyswitches(tracks: &[LTrack], acc: &mut Acc) {
    for t in tracks {
        let Some(info) = t.info.as_ref().filter(|c| !c.keyswitches.is_empty()) else { continue };
        let label = |k: u8| match info.keyswitches.get(&k).map(String::as_str) {
            Some("") | None => note_name(k),
            Some(l) => format!("{} ({l})", note_name(k)),
        };
        for n in &t.held_switches {
            let draft = Draft {
                kind: Kind::Keyswitch,
                severity: Severity::Info,
                tracks: vec![t.name().to_string()],
                message: format!("{} holds the keyswitch {} for {} beats: it switches the articulation but does not sound", t.name(), label(n.key), fmt_num(n.dur)),
                fix: "write keyswitches as blips (under 0.05 beat) a little before the notes they switch".into(),
            };
            acc.add(n.start, key(&[&t.name(), &"held", &n.key]), draft);
        }
        for b in &t.blips {
            if info.keyswitches.contains_key(&b.pitch) {
                continue;
            }
            let list: Vec<String> = info.keyswitches.keys().map(|&k| label(k)).collect();
            let draft = Draft {
                kind: Kind::Keyswitch,
                severity: Severity::Warning,
                tracks: vec![t.name().to_string()],
                message: format!("{} has a {}-beat note on {}, which is not a keyswitch of {} (they are {})", t.name(), fmt_num(b.dur), note_name(b.pitch), info.name, list.join(", ")),
                fix: "delete it, or lengthen it if it is meant to sound".into(),
            };
            acc.add(b.start, key(&[&t.name(), &"stray", &b.pitch]), draft);
        }
    }
}

/// Chord marks that cannot be read, and marks after the end of the song.
pub(crate) fn chord_symbols(harmony: &ChordTrack, song_end: f64, acc: &mut Acc) {
    for s in &harmony.spans {
        let draft = match &s.parsed {
            Err(e) => Draft {
                kind: Kind::ChordSymbol,
                severity: Severity::Error,
                tracks: vec![],
                message: format!("the chord mark at bar {} beat {} cannot be read: {e}", s.bar, fmt_num(s.beat)),
                fix: "rewrite it with the compose harmony action (e.g. Am7, F#m7b5, G7/B, Csus4, N.C.)".into(),
            },
            Ok(_) if s.start >= song_end - 1e-9 => Draft {
                kind: Kind::ChordSymbol,
                severity: Severity::Info,
                tracks: vec![],
                message: format!("the chord mark {:?} at bar {} is after the end of the song", s.text, s.bar),
                fix: "remove it, or lengthen the song".into(),
            },
            Ok(_) => continue,
        };
        acc.add(s.start, key(&[&s.bar, &s.beat, &s.text]), draft);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_names() {
        assert_eq!(interval_name(14), "14 semitones, a major 9th");
        assert_eq!(interval_name(-19), "19 semitones, a perfect 12th");
        assert_eq!(interval_name(30), "30 semitones");
    }

    #[test]
    fn controller_piles() {
        let p = piles(vec![(1.0, 5), (0.0, 1), (1.0004, 7), (2.0, 3), (0.0, 1)]);
        assert_eq!(p, [(0.0, vec![1, 1]), (1.0, vec![5, 7])]);
    }
}
