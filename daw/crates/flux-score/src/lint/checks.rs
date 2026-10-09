//! The checks ported from `tools/score_lint.py`: clash, nct, range and mud. Their messages and merge keys
//! match the Python linter's, so the two reports can be compared line by line.

use super::prep::{Heard, LTrack, passing, top_line};
use super::{Acc, Draft, Kind, Severity, TOL, pname};
use crate::harmony::ChordTrack;
use crate::meter::Meter;
use crate::theory::chord::Chord;
use crate::theory::note::note_name;
use crate::util::{OrderedMap, round_even};
use std::collections::BTreeSet;

/// A merge key from its parts (the unit separator cannot appear in names).
pub(crate) fn key(parts: &[&dyn std::fmt::Display]) -> String {
    parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("\u{1f}")
}

fn join(names: &BTreeSet<String>) -> String {
    names.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
}

/// The intervals heard as clashes.
fn clash_interval(semitones: i32) -> Option<&'static str> {
    match semitones {
        1 => Some("minor 2nd"),
        11 => Some("major 7th"),
        13 => Some("minor 9th"),
        _ => None,
    }
}

/// Tracks clashing the same way in a bar: when it first happens and the longest overlap.
struct Clashing {
    tracks: BTreeSet<String>,
    against: BTreeSet<String>,
    at: f64,
    overlap: f64,
}

impl Clashing {
    fn new(at: f64, against: BTreeSet<String>) -> Clashing {
        Clashing { tracks: BTreeSet::new(), against, at, overlap: 0.0 }
    }
}

/// Notes in two tracks sounding together for at least half a beat a minor 2nd, major 7th or minor 9th
/// apart, unless either is a short passing note (score_lint.py's `check_clashes`). Doubled parts that clash
/// the same way in a bar share one finding. A major 7th or minor 2nd between two tones of the chord the chord
/// track names for the whole overlap (A over Bb in Bbmaj7, E over F in Dm9) is that chord's colour, not a
/// clash; a minor 9th always is one.
pub(crate) fn clashes(tracks: &[LTrack], harmony: &ChordTrack, acc: &mut Acc) {
    struct Ev {
        s: f64,
        d: f64,
        p: i32,
        ti: usize,
        short: bool,
    }
    let mut ev: Vec<Ev> = Vec::new();
    for (ti, t) in tracks.iter().enumerate() {
        if t.pitched {
            let short = passing(&t.notes);
            ev.extend(t.notes.iter().zip(short).map(|(n, short)| Ev { s: n.start, d: n.dur, p: n.pitch, ti, short }));
        }
    }
    ev.sort_by(|a, b| a.s.total_cmp(&b.s).then(a.d.total_cmp(&b.d)).then(a.p.cmp(&b.p)).then(a.ti.cmp(&b.ti)).then(a.short.cmp(&b.short)));

    // (bar, high track, high pitch, low pitch) -> the low tracks, the longest overlap
    let mut found: OrderedMap<(u32, String, i32, i32), Clashing> = OrderedMap::default();
    let mut active: Vec<&Ev> = Vec::new();
    for e in &ev {
        active.retain(|a| a.s + a.d >= e.s + 0.5);
        for a in &active {
            if a.ti == e.ti || clash_interval((e.p - a.p).abs()).is_none() || e.short || a.short {
                continue;
            }
            let ov = (e.s + e.d).min(a.s + a.d) - e.s.max(a.s);
            if ov >= 0.5 && !in_chord(harmony, e.s.max(a.s), (e.s + e.d).min(a.s + a.d), a.p, e.p) {
                let ((lo, lo_p), (hi, hi_p)) = if a.p <= e.p { ((a.ti, a.p), (e.ti, e.p)) } else { ((e.ti, e.p), (a.ti, a.p)) };
                let f = found.entry_or_insert_with((acc.bar(e.s), tracks[hi].name().to_string(), hi_p, lo_p), || Clashing::new(e.s, BTreeSet::new()));
                f.tracks.insert(tracks[lo].name().to_string());
                f.overlap = f.overlap.max(ov);
            }
        }
        active.push(e);
    }
    // doubled parts (Gtr L + Gtr R) share one line
    // (bar, high pitch, low pitch, low tracks) -> the high tracks, the longest overlap
    let mut merged: OrderedMap<(u32, i32, i32, String), Clashing> = OrderedMap::default();
    for ((bar, hi_t, hi_p, lo_p), f) in found.into_entries() {
        let m = merged.entry_or_insert_with((bar, hi_p, lo_p, join(&f.tracks)), || Clashing::new(f.at, f.tracks));
        m.tracks.insert(hi_t);
        m.overlap = m.overlap.max(f.overlap);
    }
    for ((_, hi_p, lo_p, los), m) in merged.into_entries() {
        let (his_set, lo_set, ov) = (m.tracks, m.against, m.overlap);
        let his = join(&his_set);
        let interval = clash_interval(hi_p - lo_p).unwrap_or("clash");
        let draft = Draft {
            kind: Kind::Clash,
            severity: Severity::Warning,
            tracks: his_set.into_iter().chain(lo_set).collect(),
            message: format!("{his} {} against {los} {}: {interval}, {ov:.1} beats", pname(hi_p), pname(lo_p)),
            fix: "move one of the two notes to a tone of the chord, or shorten one so they overlap for less than half a beat".into(),
        };
        // score_lint.py files a clash at its bar's downbeat; the same bar, with the beat where it starts
        acc.add(m.at, key(&[&his, &hi_p, &los, &lo_p]), draft);
    }
}

/// Whether pitches `p` and `q`, a major 7th or minor 2nd apart, are both tones of the chord sounding from `from`
/// to `to` (the same chord at both ends).
fn in_chord(harmony: &ChordTrack, from: f64, to: f64, p: i32, q: i32) -> bool {
    if !matches!((p - q).abs(), 1 | 11) {
        return false;
    }
    let (Some(a), Some(b)) = (harmony.at(from + TOL), harmony.at(to - TOL)) else { return false };
    if a.start != b.start {
        return false;
    }
    a.chord().is_some_and(|c| c.contains(p.rem_euclid(12) as u8) && c.contains(q.rem_euclid(12) as u8))
}

/// The chord tone nearest to `p` (ties go down).
fn nearest_tone(p: i32, chord: &Chord) -> i32 {
    (0..=6).flat_map(|d| [p - d, p + d]).find(|q| chord.contains(q.rem_euclid(12) as u8)).unwrap_or(p)
}

/// Melody notes on strong beats (metric weight 0.6 or more) that are not chord tones of the chord track
/// (score_lint.py's `check_harmony`): an appoggiatura if the line steps to a chord tone while the chord
/// lasts, else a clash.
pub(crate) fn ncts(tracks: &[LTrack], harmony: &ChordTrack, meter: Meter, melody: &[usize], acc: &mut Acc) {
    let bar_len = acc.bar_len();
    let count_unit = 4.0 / meter.den as f64;
    for (ti, t) in tracks.iter().enumerate() {
        if !melody.contains(&ti) {
            continue;
        }
        let line = top_line(&t.notes);
        for (i, n) in line.iter().enumerate() {
            let (s, d, p) = (n.start, n.dur, n.pitch);
            let Some(span) = harmony.at(s + TOL) else { continue };
            let Some(chord) = span.chord() else { continue };
            let is_tone = |q: i32| chord.contains(q.rem_euclid(12) as u8);
            if meter.weight(s) < 0.6 || is_tone(p) {
                continue;
            }
            let count = round_even((s - (acc.bar(s) - 1) as f64 * bar_len) / count_unit) as i64 + 1;
            // the line while the note fades
            let later: Vec<&Heard> = line[i + 1..].iter().filter(|q| q.start <= s + d + 0.5).collect();
            let res = later.iter().find(|q| (1..=2).contains(&(q.pitch - p).abs()) && q.start < span.end - TOL && is_tone(q.pitch));
            let nxt = later.iter().find(|q| q.start >= s + d - 0.25);
            let tones: BTreeSet<i32> = chord.pcs().chain([chord.bass]).map(i32::from).collect();
            let near = tones.iter().map(|&q| (p - q).rem_euclid(12).min((q - p).rem_euclid(12))).min().unwrap_or(0);
            let rub = match near {
                1 => "a semitone".to_string(),
                2 => "a whole step".to_string(),
                n => format!("{n} semitones"),
            };
            let movement = match nxt {
                None => "not resolved".to_string(),
                Some(q) if q.pitch == p => "repeated".to_string(),
                Some(q) => format!(
                    "{} to {}{}",
                    if (q.pitch - p).abs() <= 2 { "steps" } else { "leaps" },
                    pname(q.pitch),
                    if q.start >= span.end - TOL { " after the chord changes" } else { "" }
                ),
            };
            let (kind, severity, how, fix) = match res {
                Some(r) => (
                    Kind::NctAppoggiatura,
                    Severity::Info,
                    format!("appoggiatura -> {}", pname(r.pitch)),
                    format!("fine if the leaning note is meant; otherwise start on {}", pname(r.pitch)),
                ),
                None => (
                    Kind::NctClash,
                    Severity::Warning,
                    format!("clash ({rub} from a chord tone), {movement}"),
                    format!(
                        "move it to a tone of {} (nearest: {}), resolve it by step while the chord lasts, or change the chord on the chord track",
                        span.text,
                        pname(nearest_tone(p, chord))
                    ),
                ),
            };
            let draft = Draft { kind, severity, tracks: vec![t.name().to_string()], message: format!("{} {} on {count} over {}: {how}", t.name(), pname(p), span.text), fix };
            acc.add(s, key(&[&t.name(), &p, &span.text, &how]), draft);
        }
    }
}

/// Notes on keys the instrument does not play (score_lint.py's `check_range`, with the keys as written and
/// the gaps between a card's key runs).
pub(crate) fn range(tracks: &[LTrack], acc: &mut Acc) {
    for t in tracks {
        let Some(info) = &t.info else { continue };
        let (lo, hi) = info.span();
        for n in &t.notes {
            let k = n.key;
            if info.plays(k) {
                continue;
            }
            let side = if k < lo { "below its playable" } else if k > hi { "above its playable" } else { "in a gap between its playable keys" };
            let octave = [12, -12, 24, -24, 36, -36].into_iter().map(|m| k as i32 + m).find(|&q| (0..=127).contains(&q) && info.plays(q as u8));
            let fix = match octave {
                Some(q) => format!("move it to {} (an octave shift that {} plays), or give it to another instrument", note_name(q as u8), info.name),
                None => "give the note to another instrument".to_string(),
            };
            let draft = Draft {
                kind: Kind::Range,
                severity: Severity::Error,
                tracks: vec![t.name().to_string()],
                message: format!("{} {} is {side} {} ({})", t.name(), note_name(k), info.describe_range(), info.source),
                fix,
            };
            acc.add(n.start, key(&[&t.name(), &k]), draft);
        }
    }
}

/// More than 3 different sustained pitches (a beat or longer) below C3 within an octave at once
/// (score_lint.py's `check_mud`).
pub(crate) fn mud(tracks: &[LTrack], acc: &mut Acc) {
    const FLOOR: i32 = 48;
    const SUSTAIN: f64 = 1.0;
    let ev: Vec<(f64, f64, i32, &str)> = tracks
        .iter()
        .filter(|t| t.pitched)
        .flat_map(|t| t.notes.iter().filter(|n| n.pitch < FLOOR && n.dur >= SUSTAIN).map(move |n| (n.start, n.end(), n.pitch, t.name())))
        .collect();
    let mut times: Vec<f64> = ev.iter().flat_map(|e| [e.0, e.1]).collect();
    times.sort_by(f64::total_cmp);
    times.dedup();
    for w in times.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b - a < 0.25 {
            continue;
        }
        let on: Vec<(i32, &str)> = ev.iter().filter(|e| e.0 <= a + 1e-6 && e.1 >= b - 1e-6).map(|e| (e.2, e.3)).collect();
        let mut ps: Vec<i32> = on.iter().map(|x| x.0).collect();
        ps.sort_unstable();
        ps.dedup();
        for i in 0..ps.len() {
            let group: Vec<i32> = ps[i..].iter().copied().filter(|&q| q - ps[i] <= 12).collect();
            if group.len() > 3 {
                let who: BTreeSet<String> = on.iter().filter(|x| group.contains(&x.0)).map(|x| x.1.to_string()).collect();
                let names: Vec<String> = group.iter().map(|&q| pname(q)).collect();
                let draft = Draft {
                    kind: Kind::Mud,
                    severity: Severity::Warning,
                    message: format!("{} at once ({})", names.join(" "), join(&who)),
                    tracks: who.into_iter().collect(),
                    fix: "thin the low register: drop a voice, move one up an octave, or keep only the bass below C3".into(),
                };
                acc.add(a, group.iter().map(i32::to_string).collect::<Vec<_>>().join(","), draft);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_formatting_rounds_half_to_even_like_python() {
        // the clash message prints overlaps with Python's "{:.1f}"
        assert_eq!(format!("{:.1}", 0.25), "0.2");
        assert_eq!(format!("{:.1}", 0.75), "0.8");
        assert_eq!(format!("{:.1}", 2.5), "2.5");
        assert_eq!(format!("{:.1}", 1.05), "1.1"); // 1.05 is 1.0500000000000000444 in binary
    }

    #[test]
    fn nearest_chord_tones() {
        let c = Chord::parse("C").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(nearest_tone(62, &c), 60); // D: C and E are equally near, ties go down
        assert_eq!(nearest_tone(65, &c), 64);
        assert_eq!(nearest_tone(66, &c), 67);
    }
}
