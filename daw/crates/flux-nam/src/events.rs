//! The events a rig's SFZ source plays: the job's notes, controllers and bends, preceded by the rig's initial
//! controllers and, for rigs with a round-robin offset, a silent pre-roll that advances every round-robin counter.
//!
//! This reproduces `render()` and `_round_robin_preroll()` of `daw/bridge/rig.py`, including the order in which
//! `sfizz_engine.SfizzSynth.render` delivers events that share a sample. The result is engine-neutral: the
//! engine renders `preroll + frames` samples from the source, feeding each [`Event`] at its frame, and drops the
//! first `preroll` samples.

use crate::rig::Source;

/// A note of the job, in seconds from the song start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JobNote {
    pub start_s: f64,
    pub end_s: f64,
    pub pitch: u8,
    pub velocity: u8,
}

/// A controller change of the job.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JobCc {
    pub time_s: f64,
    pub cc: u8,
    pub value: u8,
}

/// A pitch-bend change of the job (-8192..=8191, sfizz pitch-wheel units).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JobBend {
    pub time_s: f64,
    pub value: i32,
}

/// What happens at an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    NoteOn { key: u8, velocity: u8 },
    NoteOff { key: u8, velocity: u8 },
    Cc { cc: u8, value: u8 },
    PitchBend { value: i32 },
}

/// One event for the SFZ source at an absolute frame (counted from the start of the pre-roll).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub frame: u64,
    pub kind: EventKind,
}

/// Everything the source plays for one job.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceEvents {
    /// Frames of lead-in before the song starts. Render `preroll + frames` and discard the first `preroll`.
    pub preroll: u64,
    /// Events sorted in delivery order (frame, then controllers and bends, note-offs, keyswitch-range note-ons
    /// (keys >= 120), other note-ons; ties keep their order of creation).
    pub events: Vec<Event>,
}

/// Python's `round()` on a float: nearest integer, ties to even.
fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// An event before sorting, with a signed frame so out-of-range inputs sort as Python would sort them.
#[derive(Clone, Copy)]
struct Raw {
    frame: i64,
    kind: EventKind,
}

impl Raw {
    fn on(frame: i64, key: u8, velocity: u8) -> Raw {
        Raw { frame, kind: EventKind::NoteOn { key, velocity } }
    }
    fn off(frame: i64, key: u8) -> Raw {
        Raw { frame, kind: EventKind::NoteOff { key, velocity: 0 } }
    }

    /// `SfizzSynth.render`'s sort key after the time: (cc/pb 0, off 1, on 2), then keyswitch-range ons first.
    fn order(&self) -> (u8, u8) {
        match self.kind {
            EventKind::Cc { .. } | EventKind::PitchBend { .. } => (0, 1),
            EventKind::NoteOff { .. } => (1, 1),
            EventKind::NoteOn { key, .. } => (2, if key >= 120 { 0 } else { 1 }),
        }
    }
}

/// The round-robin pre-roll (`_round_robin_preroll`): for every keyswitch the song uses (or once, if it uses
/// none), select it, then play every non-keyswitch pitch of the song `rr_offset` times, 50 ms apart; finally
/// select `default_ks`. Returns the pre-roll length (the last step plus four seconds for releases to die away)
/// and its events. Without a round-robin offset there is no pre-roll at all (and `default_ks` is not sent).
fn round_robin_preroll(notes: &[JobNote], source: &Source, sample_rate: u32) -> (i64, Vec<Raw>) {
    let offset = source.rr_offset as usize;
    if offset == 0 {
        return (0, vec![]);
    }
    let sr = sample_rate as i64;
    let mut switches: Vec<u8> = notes.iter().map(|n| n.pitch).filter(|&p| source.is_keyswitch(p)).collect();
    switches.sort_unstable();
    switches.dedup();
    let mut pitches: Vec<u8> = notes.iter().map(|n| n.pitch).filter(|&p| !source.is_keyswitch(p)).collect();
    pitches.sort_unstable();
    pitches.dedup();
    let switches: Vec<Option<u8>> = if switches.is_empty() { vec![None] } else { switches.into_iter().map(Some).collect() };
    // int(0.05 * sr), truncating like Python
    let step = (0.05 * sample_rate as f64) as i64;
    let mut ev = vec![];
    let mut t = 0i64;
    for k in switches {
        if let Some(k) = k {
            ev.push(Raw::on(t, k, 100));
            ev.push(Raw::off(t + 10, k));
            t += step;
        }
        for _ in 0..offset {
            for &p in &pitches {
                ev.push(Raw::on(t, p, 100));
                ev.push(Raw::off(t + step / 2, p));
            }
            t += step;
        }
    }
    if let Some(k) = source.default_ks {
        ev.push(Raw::on(t, k, 100));
        ev.push(Raw::off(t + 10, k));
    }
    (t + 4 * sr, ev)
}

/// Build the source events for one job (`render()` in `rig.py`): initial controllers at frame 0, the round-robin
/// pre-roll, then the job shifted by the pre-roll. Keyswitch notes start one sample early (never before the
/// pre-roll end) so a switch and a note at the same time land in that order. Times convert to frames with
/// Python's rounding (ties to even).
pub fn source_events(source: &Source, notes: &[JobNote], ccs: &[JobCc], bends: &[JobBend], sample_rate: u32) -> SourceEvents {
    let sr = sample_rate as f64;
    let (pre, warm) = round_robin_preroll(notes, source, sample_rate);
    let mut ev: Vec<Raw> = Vec::with_capacity(source.init_cc.len() + warm.len() + 2 * notes.len() + ccs.len() + bends.len());
    ev.extend(source.init_cc.iter().map(|&(cc, value)| Raw { frame: 0, kind: EventKind::Cc { cc, value } }));
    ev.extend(warm);
    for n in notes {
        let early = if source.is_keyswitch(n.pitch) { 1 } else { 0 };
        ev.push(Raw::on(pre + (py_round(n.start_s * sr) - early).max(0), n.pitch, n.velocity));
        ev.push(Raw::off(pre + py_round(n.end_s * sr), n.pitch));
    }
    ev.extend(ccs.iter().map(|c| Raw { frame: pre + py_round(c.time_s * sr), kind: EventKind::Cc { cc: c.cc, value: c.value } }));
    ev.extend(bends.iter().map(|b| Raw { frame: pre + py_round(b.time_s * sr), kind: EventKind::PitchBend { value: b.value } }));
    // stable, like Python's sorted()
    ev.sort_by_key(|e| (e.frame, e.order()));
    SourceEvents {
        preroll: pre as u64,
        events: ev.into_iter().map(|e| Event { frame: e.frame.max(0) as u64, kind: e.kind }).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn source(rr: u32, default_ks: Option<u8>) -> Source {
        Source {
            sfz_raw: "a.sfz".into(),
            sfz: PathBuf::from("a.sfz"),
            init_cc: vec![(22, 51), (32, 0)],
            keyswitches: vec![(0, 29), (89, 127)],
            tune_cents: 0.0,
            rr_offset: rr,
            default_ks,
        }
    }

    fn note(s: f64, e: f64, p: u8) -> JobNote {
        JobNote { start_s: s, end_s: e, pitch: p, velocity: 90 }
    }

    #[test]
    fn no_offset_means_no_preroll() {
        let notes = [note(0.0, 0.02, 20), note(0.0, 0.5, 40)];
        let se = source_events(&source(0, Some(17)), &notes, &[], &[], 48000);
        assert_eq!(se.preroll, 0);
        // controllers, then the keyswitch (clamped at 0, not -1), then the note
        let k: Vec<EventKind> = se.events.iter().map(|e| e.kind).collect();
        assert_eq!(k[0], EventKind::Cc { cc: 22, value: 51 });
        assert_eq!(k[1], EventKind::Cc { cc: 32, value: 0 });
        assert_eq!(k[2], EventKind::NoteOn { key: 20, velocity: 90 });
        assert_eq!(k[3], EventKind::NoteOn { key: 40, velocity: 90 });
        assert!(!k.iter().any(|e| matches!(e, EventKind::NoteOn { key: 17, .. })), "default_ks only follows a pre-roll");
    }

    #[test]
    fn preroll_cycles_switches_then_default() {
        let notes = [note(1.0, 1.02, 20), note(1.0, 2.0, 40), note(2.0, 3.0, 47), note(2.0, 2.02, 17)];
        let sr = 44100;
        let se = source_events(&source(2, Some(17)), &notes, &[], &[], sr);
        let step = 2205i64; // int(0.05 * 44100)
        // switches 17 and 20; each: switch (1 step) + 2 offset passes (2 steps) = 3 steps
        let t_end = 6 * step;
        assert_eq!(se.preroll as i64, t_end + 4 * sr as i64);
        let ons: Vec<(u64, u8)> = se
            .events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::NoteOn { key, .. } => Some((e.frame, key)),
                _ => None,
            })
            .collect();
        let s = step as u64;
        assert_eq!(&ons[..5], &[(0, 17), (s, 40), (s, 47), (2 * s, 40), (2 * s, 47)]);
        assert_eq!(&ons[5..10], &[(3 * s, 20), (4 * s, 40), (4 * s, 47), (5 * s, 40), (5 * s, 47)]);
        assert_eq!(ons[10], (6 * s, 17));
        // the song: the keyswitch at 1.0 s goes one sample early
        let pre = se.preroll;
        assert!(ons.contains(&(pre + 44100 - 1, 20)) && ons.contains(&(pre + 44100, 40)));
    }

    #[test]
    fn rounding_and_same_frame_order_match_python() {
        // 0.5 / 44100 * 44100 lands on .5 ties: Python rounds to even
        let sr = 2;
        let notes = [note(1.25, 1.75, 60)]; // 2.5 -> 2, 3.5 -> 4
        let se = source_events(&source(0, None), &notes, &[], &[], sr);
        let on = se.events.iter().find(|e| matches!(e.kind, EventKind::NoteOn { .. })).unwrap();
        let off = se.events.iter().find(|e| matches!(e.kind, EventKind::NoteOff { .. })).unwrap();
        assert_eq!((on.frame, off.frame), (2, 4));
        // same frame: cc/pb, then offs, then ons with key >= 120, then other ons
        let notes = [note(0.0, 1.0, 60), note(1.0, 2.0, 121), note(1.0, 2.0, 62)];
        let ccs = [JobCc { time_s: 1.0, cc: 1, value: 5 }];
        let bends = [JobBend { time_s: 1.0, value: 100 }];
        let mut s = source(0, None);
        s.init_cc.clear();
        s.keyswitches.clear();
        let se = source_events(&s, &notes, &ccs, &bends, 10);
        let at10: Vec<EventKind> = se.events.iter().filter(|e| e.frame == 10).map(|e| e.kind).collect();
        assert_eq!(
            at10,
            vec![
                EventKind::Cc { cc: 1, value: 5 },
                EventKind::PitchBend { value: 100 },
                EventKind::NoteOff { key: 60, velocity: 0 },
                EventKind::NoteOn { key: 121, velocity: 90 },
                EventKind::NoteOn { key: 62, velocity: 90 },
            ]
        );
    }
}
