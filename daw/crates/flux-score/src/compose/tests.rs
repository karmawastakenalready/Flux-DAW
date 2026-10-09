use super::*;
use flux_core::commands::{BendSpec, CcSpec, apply};
use flux_core::{BendPoint, CcPoint, Instrument};

fn project(bars: u32, ts: (u32, u32), key: &str) -> Project {
    let mut p: Project = serde_json::from_value(json!({
        "name": "test", "tempo": [{"bar": 1, "bpm": 100.0}], "time_signature": [ts.0, ts.1], "bars": bars, "key": key,
    }))
    .expect("minimal project");
    for name in ["Lead", "Pad", "Bass"] {
        apply(&mut p, &Command::AddTrack { name: name.into(), instrument: Instrument::None, color: None }).expect("add track");
    }
    p
}

fn run(p: &Project, args: Value) -> ComposeOutput {
    compose(p, &args).unwrap_or_else(|e| panic!("{args}: {e}"))
}

/// Applies the output's commands to a copy of the project, like one undo step would.
fn applied(p: &Project, out: &ComposeOutput) -> Project {
    let mut q = p.clone();
    for (i, c) in out.commands.iter().enumerate() {
        apply(&mut q, c).unwrap_or_else(|e| panic!("command {} ({}): {e}", i + 1, c.describe()));
    }
    q
}

fn run_apply(p: &Project, args: Value) -> (Project, ComposeOutput) {
    let out = run(p, args);
    (applied(p, &out), out)
}

fn track<'p>(p: &'p Project, name: &str) -> &'p Track {
    &p.tracks[p.track_index(&TrackRef::Name(name.into())).expect("track")]
}

fn err(p: &Project, args: Value) -> String {
    compose(p, &args).expect_err("should fail").to_string()
}

fn same_data(a: &Track, b: &Track) {
    assert_eq!(a.notes, b.notes);
    assert_eq!(a.ccs, b.ccs);
    assert_eq!(a.bends, b.bends);
}

#[test]
fn chords_from_a_chart_write_the_chord_track_and_voicings() {
    let p = project(8, (4, 4), "A minor");
    let args = json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 4, "chords": "Am | F | C | G", "pattern": "x--x --x-", "step": 0.5});
    let (q, out) = run_apply(&p, args.clone());
    assert!(matches!(out.commands[0], Command::SetHarmony { .. }));
    let marks: Vec<(u32, &str)> = q.harmony.iter().map(|m| (m.bar, m.chord.as_str())).collect();
    assert_eq!(marks, [(1, "Am"), (2, "F"), (3, "C"), (4, "G")]);
    let pad = track(&q, "Pad");
    // 3 strikes per bar, 4 voices
    assert_eq!(pad.notes.len(), 4 * 3 * 4);
    assert!(pad.notes.iter().all(|n| (48..=79).contains(&n.pitch)));
    // strikes at 0, 1.5, 3 in each bar, held to the next strike
    let mut starts: Vec<f64> = pad.notes.iter().filter(|n| n.start < 4.0).map(|n| n.start).collect();
    starts.dedup();
    assert_eq!(starts, [0.0, 1.5, 3.0]);
    let first: Vec<&Note> = pad.notes.iter().filter(|n| n.start == 0.0).collect();
    assert!(first.iter().all(|n| n.dur == 1.5));
    // the downbeat is accented by the metre
    let off = pad.notes.iter().find(|n| n.start == 1.5).unwrap();
    assert!(first[0].vel > off.vel);
    // every voicing holds the chord's tones
    let am: Vec<u8> = first.iter().map(|n| n.pitch % 12).collect();
    assert!([9, 0, 4].iter().all(|pc| am.contains(pc)));
    assert_eq!(out.summary["chords"].as_array().unwrap().len(), 4);
    assert_eq!(out.summary["notes"], 48);
    // re-running gives the same project (and no SetHarmony: the chord track is already right)
    let (r, again) = run_apply(&q, args);
    assert!(!again.commands.iter().any(|c| matches!(c, Command::SetHarmony { .. })));
    same_data(track(&q, "Pad"), track(&r, "Pad"));
    assert_eq!(again.summary["removed"], 48);
}

#[test]
fn chords_read_the_existing_chord_track_and_lead_on() {
    let mut p = project(8, (4, 4), "C major");
    apply(&mut p, &Command::SetHarmony { chords: vec![ChordMark { bar: 1, beat: 0.0, chord: "C".into() }, ChordMark { bar: 2, beat: 0.0, chord: "G7/B".into() }, ChordMark { bar: 3, beat: 0.0, chord: "C".into() }] }).unwrap();
    let (q, out) = run_apply(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 2}));
    assert_eq!(out.commands.len(), 1, "no chart, nothing to replace: only the notes");
    assert_eq!(track(&q, "Pad").notes.len(), 8);
    // the next call leads on from the last chord before its range
    let (_, out2) = run_apply(&q, json!({"action": "chords", "track": "Pad", "from_bar": 3, "to_bar": 3}));
    assert!(out2.summary["start"].as_str().unwrap().contains("bar 2"));
    // the chord track repeats C in bars 3-8: one strike, held
    let (r, _) = run_apply(&p, json!({"action": "chords", "track": "Pad", "from_bar": 3, "to_bar": 8, "voices": 3}));
    let pad = track(&r, "Pad");
    assert_eq!(pad.notes.len(), 3);
    assert!(pad.notes.iter().all(|n| n.dur == 24.0));
}

#[test]
fn chords_without_harmony_or_with_bad_input_fail_clearly() {
    let p = project(8, (4, 4), "");
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 2})).contains("no chord at bar 1"));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 9, "chords": "C"})).contains("past the end of the song"));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 2, "chords": "C | F | G"})).contains("3 bars"));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 2, "chords": "C | Hm"})).contains("chord chart"));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 2, "chords": "C", "pattern": "x-o-"})).contains("pattern"));
    assert!(err(&p, json!({"action": "chords", "track": "Organ", "from_bar": 1, "to_bar": 2, "chords": "C"})).contains("#2 \"Pad\""));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 2, "chord": "C"})).contains("unknown field `chord`"));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 2, "chords": "C", "low": 70, "high": 60})).contains("low"));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 3, "to_bar": 2, "chords": "C"})).contains("after to_bar"));
    assert!(err(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 1, "chords": "Cmaj7", "voices": 4, "low": 60, "high": 62})).contains("voicing"));
    assert!(err(&p, json!({"action": "dance"})).contains("unknown action"));
    assert!(err(&p, json!({"track": "Pad"})).contains("\"action\" is required"));
    assert!(err(&p, json!([1, 2])).contains("JSON object"));
    // errors name the action
    assert!(err(&p, json!({"action": "bass", "track": "Bass", "from_bar": 1, "to_bar": 2})).starts_with("compose bass: "));
}

#[test]
fn chart_repeats_to_fill_the_range_and_restores_what_follows() {
    let mut p = project(8, (4, 4), "");
    apply(&mut p, &Command::SetHarmony { chords: vec![ChordMark { bar: 1, beat: 0.0, chord: "Dm".into() }] }).unwrap();
    let (q, _) = run_apply(&p, json!({"action": "harmony", "chords": "C G", "from_bar": 3, "to_bar": 4}));
    let marks: Vec<(u32, f64, &str)> = q.harmony.iter().map(|m| (m.bar, m.beat, m.chord.as_str())).collect();
    assert_eq!(marks, [(1, 0.0, "Dm"), (3, 0.0, "C"), (3, 2.0, "G"), (4, 0.0, "C"), (4, 2.0, "G"), (5, 0.0, "Dm")]);
    // without to_bar the chart sets its own length
    let (q, out) = run_apply(&p, json!({"action": "harmony", "chords": "Am | F |", "from_bar": 7}));
    assert_eq!(out.summary["bars"], json!([7, 8]));
    assert_eq!(q.harmony.last().unwrap().chord, "F");
}

#[test]
fn harmony_reads_back_tones_scales_and_avoid_notes() {
    let mut p = project(4, (4, 4), "A minor");
    apply(&mut p, &Command::SetHarmony { chords: vec![ChordMark { bar: 2, beat: 0.0, chord: "Am".into() }, ChordMark { bar: 3, beat: 0.0, chord: "E7".into() }, ChordMark { bar: 4, beat: 0.0, chord: "Hm".into() }] }).unwrap();
    let out = run(&p, json!({"action": "harmony"}));
    assert!(out.commands.is_empty());
    let chords = out.summary["chords"].as_array().unwrap();
    assert_eq!(chords[0]["tones"], "A C E");
    assert_eq!(chords[0]["scale"], "A B C D E F G");
    assert_eq!(chords[0]["avoid"], json!(["F"]));
    // E7 in A minor: the chord's G# replaces G (harmonic minor)
    assert_eq!(chords[1]["scale"], "E F G# A B C D");
    assert!(chords[2]["error"].as_str().unwrap().contains("Hm"));
    assert_eq!(out.summary["no_harmony_until"], json!({"bar": 2, "beat": 0.0}));
    assert_eq!(out.warnings.len(), 2);
}

#[test]
fn bass_lines_follow_inversions_and_patterns() {
    let p = project(4, (4, 4), "C major");
    let (q, out) = run_apply(&p, json!({"action": "bass", "track": "Bass", "from_bar": 1, "to_bar": 2, "chords": "C | G/B", "pattern": "xo5x"}));
    let bass = track(&q, "Bass");
    let bar1: Vec<u8> = bass.notes.iter().filter(|n| n.start < 4.0).map(|n| n.pitch).collect();
    let root = bar1[0];
    assert_eq!(root % 12, 0);
    assert_eq!(bar1, [root, root + 12, root + 7, root]);
    let bar2: Vec<u8> = bass.notes.iter().filter(|n| n.start >= 4.0).map(|n| n.pitch).collect();
    assert_eq!(bar2[0] % 12, 11, "G/B puts B in the bass");
    assert!(bass.notes.iter().all(|n| n.dur == 1.0));
    assert_eq!(out.summary["chords"][1]["notes"][0].as_str().unwrap().chars().next(), Some('B'));
    // a single start pitch only
    assert!(err(&p, json!({"action": "bass", "track": "Bass", "from_bar": 1, "to_bar": 1, "chords": "C", "start": [40, 43]})).contains("one note"));
}

#[test]
fn arpeggios_walk_the_voicing() {
    let p = project(2, (4, 4), "");
    let (q, out) = run_apply(&p, json!({"action": "arpeggio", "track": "Pad", "from_bar": 1, "to_bar": 2, "chords": "C | N.C."}));
    assert_eq!(out.summary["pattern"], "012321");
    let pad = track(&q, "Pad");
    // eighths through bar 1, silence over N.C.
    assert_eq!(pad.notes.len(), 8);
    let ps: Vec<u8> = pad.notes.iter().map(|n| n.pitch).collect();
    assert!(ps[0] < ps[1] && ps[1] < ps[2] && ps[2] < ps[3] && ps[4] == ps[2] && ps[6] == ps[0]);
    // index 4 is the bottom an octave up
    let (q, _) = run_apply(&p, json!({"action": "arpeggio", "track": "Pad", "from_bar": 1, "to_bar": 1, "chords": "C", "pattern": "04", "step": 2, "voices": 4}));
    let ps: Vec<u8> = track(&q, "Pad").notes.iter().map(|n| n.pitch).collect();
    assert_eq!(ps[1], ps[0] + 12);
}

#[test]
fn melody_replaces_its_bars_and_keeps_keyswitches() {
    let mut p = project(4, (4, 4), "E minor");
    let lead = TrackRef::Name("Lead".into());
    let old = |start: f64, dur: f64, pitch: u8| NoteSpec { bar: None, beat: start, dur, pitch: PitchSpec::Num(pitch), vel: 80 };
    apply(&mut p, &Command::AddNotes { track: lead, notes: vec![old(0.0, 0.02, 24), old(1.0, 1.0, 70), old(8.0, 1.0, 72)] }).unwrap();
    let args = json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "E4:2 G4:1 B4:1 | A4:3 r:1"});
    let (q, out) = run_apply(&p, args.clone());
    let notes = &track(&q, "Lead").notes;
    let pitches: Vec<u8> = notes.iter().map(|n| n.pitch).collect();
    assert_eq!(pitches, [24, 64, 67, 71, 69, 72], "the blip and bar 3 stay");
    assert_eq!(out.summary["removed"], 1);
    assert!(matches!(out.commands[0], Command::DeleteNotes { .. }), "a blip in range: exact deletes");
    let (r, _) = run_apply(&q, args);
    same_data(track(&q, "Lead"), track(&r, "Lead"));
    // phrasing, and the notation is checked
    let out = run(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "E4:2 G4:1 B4:1 | A4:3 r:1", "phrase": {"vary": 0}}));
    assert_eq!(out.summary["phrased"], true);
    assert!(err(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "E4:2 G4:1"})).contains("adds up to 3"));
    assert!(err(&p, json!({"action": "melody", "track": "Lead", "bar": 4, "notation": "E4:4 | G4:4"})).contains("past the end"));
    assert!(err(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "E4:4", "phrase": {"ritt": 1}})).contains("ritt"));
}

#[test]
fn melody_reports_strong_beat_clashes() {
    let mut p = project(2, (4, 4), "C major");
    apply(&mut p, &Command::SetHarmony { chords: vec![ChordMark { bar: 1, beat: 0.0, chord: "C".into() }] }).unwrap();
    let out = run(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "F4:2 E4:1 D4:1 | C#4:4"}));
    assert_eq!(out.summary["fit"]["avoid"], 1);
    assert_eq!(out.summary["fit"]["outside"], 1);
    assert_eq!(out.warnings.len(), 2, "{:?}", out.warnings);
    assert!(out.warnings[0].starts_with("bar 1: F4 is an avoid note (11) over C"));
}

#[test]
fn melody_in_compound_meter_defaults_to_eighths() {
    let p = project(2, (6, 8), "");
    let (q, out) = run_apply(&p, json!({"action": "melody", "track": "Lead", "bar": 2, "notation": "C5:3 D5:3"}));
    assert_eq!(out.summary["unit"], 0.5);
    let starts: Vec<f64> = track(&q, "Lead").notes.iter().map(|n| n.start).collect();
    assert_eq!(starts, [3.0, 4.5]);
}

#[test]
fn phrase_with_a_base_velocity_is_repeatable() {
    let p = project(2, (4, 4), "");
    let (q, _) = run_apply(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "C4:1 D4:1 E4:1 F4:1 | G4:4"}));
    let args = json!({"action": "phrase", "track": "Lead", "from_bar": 1, "to_bar": 2, "vel": 80, "seed": 7});
    let (r, out) = run_apply(&q, args.clone());
    assert_eq!(out.summary["notes"], 5);
    assert_ne!(track(&q, "Lead").notes, track(&r, "Lead").notes);
    let (s, _) = run_apply(&r, args);
    same_data(track(&r, "Lead"), track(&s, "Lead"));
    assert!(err(&p, json!({"action": "phrase", "track": "Lead", "from_bar": 1, "to_bar": 2})).contains("no notes"));
}

#[test]
fn swell_replaces_its_cc_only() {
    let mut p = project(4, (4, 4), "");
    let (q, _) = run_apply(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "C4:4 | E4:2 G4:2"}));
    p = q;
    let lead = TrackRef::Name("Lead".into());
    let cc = |cc: u8, beat: f64| Command::AddCc { track: lead.clone(), cc, points: vec![CcSpec { bar: None, beat, value: 5 }] };
    for c in [cc(1, 2.0), cc(11, 2.0), cc(1, 12.0)] {
        apply(&mut p, &c).unwrap();
    }
    let args = json!({"action": "swell", "track": "Lead", "from_bar": 1, "to_bar": 2});
    let (q, out) = run_apply(&p, args.clone());
    assert_eq!(out.summary["replaced"], 1);
    let ccs = &track(&q, "Lead").ccs;
    assert!(ccs.contains(&CcPoint { beat: 2.0, cc: 11, value: 5 }), "other controllers stay");
    assert!(ccs.contains(&CcPoint { beat: 12.0, cc: 1, value: 5 }), "CC1 outside the bars stays");
    assert!(!ccs.contains(&CcPoint { beat: 2.0, cc: 1, value: 5 }));
    assert!(ccs.iter().filter(|c| c.cc == 1 && c.beat < 8.0).count() > 10);
    let (r, _) = run_apply(&q, args);
    same_data(track(&q, "Lead"), track(&r, "Lead"));
    assert!(err(&p, json!({"action": "swell", "track": "Pad", "from_bar": 1, "to_bar": 2})).contains("no notes"));
    // following another track
    let (s, out) = run_apply(&p, json!({"action": "swell", "track": "Pad", "source": "Lead", "from_bar": 1, "to_bar": 2, "cc": 11}));
    assert_eq!(out.summary["source"], "Lead");
    assert!(!track(&s, "Pad").ccs.is_empty());
}

#[test]
fn cc_curves_replace_their_span() {
    let mut p = project(4, (4, 4), "");
    let pad = TrackRef::Name("Pad".into());
    apply(&mut p, &Command::AddCc { track: pad, cc: 1, points: vec![CcSpec { bar: None, beat: 0.5, value: 9 }, CcSpec { bar: None, beat: 2.0, value: 9 }, CcSpec { bar: None, beat: 7.5, value: 9 }] }).unwrap();
    let args = json!({"action": "curve", "track": "Pad", "cc": 1, "points": [{"bar": 1, "beat": 1, "value": 0}, {"bar": 2, "beat": 2, "value": 100}], "shape": "smooth"});
    let (q, out) = run_apply(&p, args.clone());
    assert_eq!(out.summary["replaced"], 1);
    let ccs = &track(&q, "Pad").ccs;
    assert!(ccs.contains(&CcPoint { beat: 0.5, cc: 1, value: 9 }) && ccs.contains(&CcPoint { beat: 7.5, cc: 1, value: 9 }));
    assert!(!ccs.contains(&CcPoint { beat: 2.0, cc: 1, value: 9 }));
    assert_eq!(ccs.iter().find(|c| c.beat == 6.0).map(|c| c.value), Some(100));
    let (r, _) = run_apply(&q, args);
    same_data(track(&q, "Pad"), track(&r, "Pad"));
    assert!(err(&p, json!({"action": "curve", "track": "Pad", "points": [{"beat": 0, "value": 0}]})).contains("give cc"));
    assert!(err(&p, json!({"action": "curve", "track": "Pad", "cc": 1, "points": [{"beat": 2, "value": 0}, {"beat": 1, "value": 3}]})).contains("time order"));
    assert!(err(&p, json!({"action": "curve", "track": "Pad", "cc": 1, "points": [{"beat": 0, "value": 200}]})).contains("outside 0-127"));
}

#[test]
fn bend_curves_keep_notes_and_ccs_exactly() {
    let mut p = project(4, (4, 4), "");
    let (q, _) = run_apply(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "C4:1/3 D4:1/3 E4:1/3 F4:3 | G4:4"}));
    p = q;
    let lead = TrackRef::Name("Lead".into());
    apply(&mut p, &Command::AddCc { track: lead.clone(), cc: 7, points: vec![CcSpec { bar: None, beat: 1.25, value: 64 }] }).unwrap();
    apply(&mut p, &Command::AddBend { track: lead, points: vec![BendSpec { bar: None, beat: 0.25, value: 100 }, BendSpec { bar: None, beat: 2.0, value: 300 }, BendSpec { bar: None, beat: 6.0, value: -50 }] }).unwrap();
    let args = json!({"action": "curve", "track": "Lead", "bend": true, "points": [{"beat": 1, "value": 0}, {"beat": 3, "value": 4096}, {"beat": 4, "value": 0}]});
    let (q, out) = run_apply(&p, args.clone());
    let (before, after) = (track(&p, "Lead"), track(&q, "Lead"));
    assert_eq!(before.notes, after.notes);
    assert_eq!(before.ccs, after.ccs);
    assert!(after.bends.contains(&BendPoint { beat: 0.25, value: 100 }) && after.bends.contains(&BendPoint { beat: 6.0, value: -50 }));
    assert!(!after.bends.contains(&BendPoint { beat: 2.0, value: 300 }));
    assert!(after.bends.contains(&BendPoint { beat: 3.0, value: 4096 }));
    assert_eq!(out.summary["replaced"], 1);
    let (r, _) = run_apply(&q, args);
    same_data(after, track(&r, "Lead"));
}

#[test]
fn fit_flags_and_fixes() {
    let mut p = project(2, (4, 4), "C major");
    apply(&mut p, &Command::SetHarmony { chords: vec![ChordMark { bar: 1, beat: 0.0, chord: "C".into() }, ChordMark { bar: 2, beat: 0.0, chord: "G7".into() }] }).unwrap();
    let (q, _) = run_apply(&p, json!({"action": "melody", "track": "Lead", "bar": 1, "notation": "F4:1 E4:1 E4:1 D#4:1 | G#4:2 B4:1 A4:1"}));
    let out = run(&q, json!({"action": "fit", "track": "Lead"}));
    assert!(out.commands.is_empty());
    assert_eq!(out.summary["counts"], json!({"chord_tones": 3, "tensions": 1, "avoid": 1, "outside": 2, "unchecked": 0}));
    // F4 over C is an avoid note; D#4 over C and G#4 over G7 (b9, not in C major) are outside
    let issues = out.summary["issues"].as_array().unwrap();
    let flagged: Vec<(&str, &str, bool)> = issues.iter().map(|i| (i["note"].as_str().unwrap(), i["class"].as_str().unwrap(), i["strong"].as_bool().unwrap())).collect();
    assert_eq!(flagged, [("F4", "avoid", true), ("D#4", "outside", false), ("G#4", "outside", true)]);
    assert_eq!(issues[0]["nearest_chord_tone"], "E4");
    // fix strong: the downbeat notes move; D#4 on beat 4 stays
    let (r, out) = run_apply(&q, json!({"action": "fit", "track": "Lead", "fix": "strong"}));
    assert_eq!(out.summary["fixed"], 2);
    let ps: Vec<u8> = track(&r, "Lead").notes.iter().map(|n| n.pitch).collect();
    assert_eq!(ps, [64, 64, 64, 63, 67, 71, 69]);
    let (r, _) = run_apply(&q, json!({"action": "fit", "track": "Lead", "fix": "all"}));
    let ps: Vec<u8> = track(&r, "Lead").notes.iter().map(|n| n.pitch).collect();
    assert_eq!(ps, [64, 64, 64, 64, 67, 71, 69]);
    // a transposing track is judged by what it sounds: written D#4 a semitone up is E4
    let out = run(&q, json!({"action": "fit", "track": "Lead", "transpose": 1, "from_bar": 1, "to_bar": 1}));
    assert_eq!(out.summary["counts"]["chord_tones"], 1);
    assert!(err(&project(2, (4, 4), ""), json!({"action": "fit", "track": "Lead"})).contains("chord track is empty"));
}

#[test]
fn unreadable_project_keys_are_reported_not_fatal() {
    let mut p = project(2, (4, 4), "Q lydian");
    apply(&mut p, &Command::SetHarmony { chords: vec![ChordMark { bar: 1, beat: 0.0, chord: "C".into() }] }).unwrap();
    let out = run(&p, json!({"action": "harmony"}));
    assert!(out.warnings.iter().any(|w| w.contains("Q lydian")));
    let out = run(&p, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 1}));
    assert!(out.warnings.is_empty(), "chords do not depend on the key");
}

#[test]
fn schema_lists_every_action_and_output_serialises() {
    let tool = compose_tool();
    assert_eq!(tool["name"], "compose");
    let actions: Vec<&str> = tool["inputSchema"]["properties"]["action"]["enum"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    assert_eq!(actions, ACTIONS);
    let p = project(2, (4, 4), "");
    let out = run(&p, json!({"action": "chords", "track": 2, "from_bar": 1, "to_bar": 1, "chords": "C"}));
    let v = out.to_json();
    assert_eq!(v["commands"], 2);
    assert_eq!(out.label, "compose chords: chords on Pad bar 1");
    // the commands are plain flux-core commands: they round-trip through JSON
    let text = serde_json::to_string(&out.commands).unwrap();
    let back: Vec<Command> = serde_json::from_str(&text).unwrap();
    assert_eq!(back.len(), 2);
}
