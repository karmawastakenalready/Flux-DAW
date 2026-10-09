use super::*;
use flux_core::{BendPoint, CcPoint, ChordMark};
use serde_json::json;

/// Notes as (start, dur, pitch).
type Notes<'a> = &'a [(f64, f64, u8)];

/// A project with tracks of notes.
fn project(ts: (u32, u32), bars: u32, tracks: &[(&str, Notes)]) -> Project {
    let tracks: Vec<Value> = tracks
        .iter()
        .enumerate()
        .map(|(i, (name, notes))| {
            let notes: Vec<Value> = notes.iter().map(|&(s, d, p)| json!({"start": s, "dur": d, "pitch": p, "vel": 90})).collect();
            json!({"id": i + 1, "name": name, "instrument": {"kind": "none"}, "notes": notes})
        })
        .collect();
    serde_json::from_value(json!({
        "name": "test", "tempo": [{"bar": 1, "bpm": 120.0}], "time_signature": [ts.0, ts.1], "bars": bars, "tracks": tracks,
    }))
    .unwrap_or_else(|e| panic!("test project: {e}"))
}

fn with_chords(mut p: Project, chords: &[(u32, f64, &str)]) -> Project {
    p.harmony = chords.iter().map(|&(bar, beat, c)| ChordMark { bar, beat, chord: c.into() }).collect();
    p
}

fn only(checks: &[Check]) -> LintOptions {
    LintOptions { checks: Some(checks.to_vec()), ..LintOptions::default() }
}

fn messages(r: &LintReport, kind: Kind) -> Vec<String> {
    r.findings.iter().filter(|f| f.kind == kind).map(|f| f.message.clone()).collect()
}

fn name(n: &str) -> TrackRef {
    TrackRef::Name(n.into())
}

#[test]
fn sustained_clashes_are_merged_across_bars_and_doubled_parts() {
    // Flute F5 over Horns E4 (a minor 9th) in bars 1 and 3; Gtr L and Gtr R double the horn line
    let p = project((4, 4), 4, &[("Flute", &[(0.0, 2.0, 77), (8.0, 2.0, 77)]), ("Gtr L", &[(0.0, 4.0, 64), (8.0, 4.0, 64)]), ("Gtr R", &[(0.0, 4.0, 64), (8.0, 4.0, 64)])]);
    let r = lint(&p, &only(&[Check::Clash]));
    assert_eq!(messages(&r, Kind::Clash), ["Flute F5 against Gtr L, Gtr R E4: minor 9th, 2.0 beats"]);
    let f = &r.findings[0];
    assert_eq!((f.bar, f.beat, f.also_bars.clone(), f.severity), (1, 0.0, vec![3], Severity::Warning));
    assert_eq!(f.tracks, ["Flute", "Gtr L", "Gtr R"]);
    assert_eq!(r.count(Kind::Clash), 2);
    // a short passing note does not clash; neither does a quick overlap
    let p = project((4, 4), 2, &[("A", &[(0.0, 0.5, 60), (0.5, 0.5, 62), (1.0, 1.0, 64)]), ("B", &[(0.0, 2.0, 61)]), ("C", &[(1.75, 1.0, 65)])]);
    assert!(lint(&p, &only(&[Check::Clash])).findings.is_empty());
}

#[test]
fn a_major_seventh_inside_the_named_chord_is_its_colour() {
    // Flute A5 over Horn Bb4 for two beats, then E5 over F4: major 7ths
    let p = project((4, 4), 2, &[("Flute", &[(0.0, 2.0, 81), (4.0, 2.0, 76)]), ("Horn", &[(0.0, 2.0, 70), (4.0, 2.0, 65)])]);
    let clashes = |p: &Project| messages(&lint(p, &only(&[Check::Clash])), Kind::Clash);
    assert_eq!(clashes(&p).len(), 2, "no chord track: both are clashes");
    // Bbmaj7 and Dm9 hold both notes of each
    assert!(clashes(&with_chords(p.clone(), &[(1, 0.0, "Bbmaj7"), (2, 0.0, "Dm9")])).is_empty());
    // a plain Bb and Dm don't; nor does a chord that changes under the overlap
    assert_eq!(clashes(&with_chords(p.clone(), &[(1, 0.0, "Bb"), (2, 0.0, "Dm")])).len(), 2);
    assert_eq!(clashes(&with_chords(p.clone(), &[(1, 0.0, "Bbmaj7"), (1, 1.0, "Gm7"), (2, 0.0, "Dm9")])).len(), 1);
    // a minor 9th is a clash even when both are chord tones: B4 over A#3 in G7#9
    let p = with_chords(project((4, 4), 1, &[("Flute", &[(0.0, 2.0, 71)]), ("Horn", &[(0.0, 2.0, 58)])]), &[(1, 0.0, "G7#9")]);
    let g = crate::harmony::ChordTrack::from_project(&p);
    assert!(g.chord_at(0.0).is_some_and(|c| c.contains(11) && c.contains(10)), "G7#9 should hold B and A#");
    assert_eq!(clashes(&p).len(), 1);
}

#[test]
fn strong_beat_non_chord_tones_need_the_chord_track() {
    // over C: D5 on beat 1 steps down to C5 (an appoggiatura); F5 on beat 3 leaps away (a clash)
    let notes: &[(f64, f64, u8)] = &[(0.0, 1.0, 74), (1.0, 1.0, 72), (2.0, 1.0, 77), (3.0, 1.0, 69)];
    let p = project((4, 4), 2, &[("Flute", notes)]);
    let r = lint(&p, &LintOptions::default());
    assert_eq!(r.count(Kind::NctAppoggiatura) + r.count(Kind::NctClash), 0);
    assert!(r.notes.iter().any(|n| n.contains("chord track is empty")), "{:?}", r.notes);

    let p = with_chords(p, &[(1, 0.0, "C")]);
    let r = lint(&p, &only(&[Check::Nct]));
    assert_eq!(r.melody, ["Flute"]);
    assert!(r.melody_guessed);
    assert_eq!(messages(&r, Kind::NctAppoggiatura), ["Flute D5 on 1 over C: appoggiatura -> C5"]);
    assert_eq!(messages(&r, Kind::NctClash), ["Flute F5 on 3 over C: clash (a semitone from a chord tone), leaps to A4"]);
    let clash = r.findings.iter().find(|f| f.kind == Kind::NctClash).expect("nct clash");
    assert_eq!((clash.bar, clash.beat, clash.severity), (1, 2.0, Severity::Warning));
    assert!(clash.fix.contains("nearest: E5"), "{}", clash.fix);
    // the slash bass counts as a chord tone; "N.C." and unreadable marks have no chord tones to test
    let p = with_chords(p, &[(1, 0.0, "C/D"), (1, 2.0, "N.C.")]);
    assert!(lint(&p, &only(&[Check::Nct])).findings.is_empty());
}

#[test]
fn ranges_come_from_cards_or_the_caller_and_use_written_keys() {
    let p = project((4, 4), 2, &[("Cello", &[(0.0, 1.0, 30), (1.0, 1.0, 50), (2.0, 1.0, 80), (3.0, 1.0, 86)])]);
    let card = InstrumentInfo { paths: vec!["sfz/cello.sfz".into()], ranges: vec![(36, 77), (84, 90)], ..InstrumentInfo::with_range("cello", 0, 0) };
    let mut p2 = p.clone();
    p2.tracks[0].instrument = Instrument::Sfz { path: "C:\\x\\sfz\\Cello.sfz".into(), init_cc: Default::default() };
    let opts = LintOptions { catalog: Some(InstrumentCatalog::new(vec![card])), transpose: vec![(name("Cello"), 12)], ..only(&[Check::Range]) };
    let r = lint(&p2, &opts);
    assert_eq!(r.ranges.matched, ["Cello"]);
    assert_eq!(
        messages(&r, Kind::Range),
        ["Cello F#1 is below its playable C2-F5, C6-F#6 (cello)", "Cello G#5 is in a gap between its playable keys C2-F5, C6-F#6 (cello)"]
    );
    assert!(r.findings[0].fix.contains("F#2"), "{}", r.findings[0].fix);
    assert_eq!(r.findings[0].severity, Severity::Error);
    // a caller's range overrides the card
    let opts = LintOptions { ranges: vec![(name("cello"), 28, 90)], ..opts };
    assert!(lint(&p2, &opts).findings.is_empty());
    // no card, no range: nothing to check
    let r = lint(&p, &only(&[Check::Range]));
    assert!(r.findings.is_empty());
    assert_eq!(r.ranges.unmatched, ["Cello"]);
}

#[test]
fn muddy_low_clusters() {
    let p = project((4, 4), 2, &[("Pad", &[(0.0, 4.0, 36), (0.0, 4.0, 40), (0.0, 4.0, 43)]), ("Bass", &[(0.0, 2.0, 38), (2.0, 2.0, 28)])]);
    let r = lint(&p, &only(&[Check::Mud]));
    assert_eq!(messages(&r, Kind::Mud), ["C2 D2 E2 G2 at once (Bass, Pad)"]);
    assert_eq!(r.findings[0].tracks, ["Bass", "Pad"]);
}

#[test]
fn parallel_fifths_and_octaves_between_melody_and_bass() {
    let p = project(
        (4, 4),
        2,
        &[("Lead", &[(0.0, 1.0, 72), (1.0, 1.0, 74), (2.0, 1.0, 79), (3.0, 1.0, 81)]), ("Bass", &[(0.0, 1.0, 48), (1.0, 1.0, 50), (2.0, 1.0, 48), (3.0, 1.0, 50)])],
    );
    let r = lint(&p, &only(&[Check::Parallels]));
    assert_eq!(messages(&r, Kind::ParallelOctaves), ["Lead and Bass move in parallel octaves: C5/C3 -> D5/D3"]);
    assert_eq!(messages(&r, Kind::ParallelFifths), ["Lead and Bass move in parallel fifths: G5/C3 -> A5/D3"]);
    // contrary motion is fine
    let p = project((4, 4), 2, &[("Lead", &[(0.0, 1.0, 72), (1.0, 1.0, 71)]), ("Bass", &[(0.0, 1.0, 48), (1.0, 1.0, 50)])]);
    assert!(lint(&p, &only(&[Check::Parallels])).findings.is_empty());
}

#[test]
fn crossings_and_leaps() {
    let high: &[(f64, f64, u8)] = &[(0.0, 1.0, 76), (1.0, 1.0, 74), (2.0, 1.0, 72), (3.0, 1.0, 74), (4.0, 1.0, 76), (5.0, 1.0, 77)];
    let low: &[(f64, f64, u8)] = &[(0.0, 1.0, 60), (1.0, 1.0, 62), (2.0, 1.0, 64), (3.0, 1.0, 76), (4.0, 1.0, 60), (5.0, 1.0, 62)];
    let p = project((4, 4), 2, &[("Violin", high), ("Viola", low)]);
    let r = lint(&p, &only(&[Check::Crossing, Check::Leaps]));
    assert_eq!(messages(&r, Kind::VoiceCrossing), ["Viola E5 crosses above Violin D5"]);
    assert_eq!(messages(&r, Kind::Leap), ["Viola leaps 16 semitones, a major 10th from E5 to C4"]);
    let r = lint(&p, &LintOptions { max_leap: 16, ..only(&[Check::Leaps]) });
    assert!(r.findings.is_empty());
}

#[test]
fn duplicate_and_overlapping_notes() {
    let p = project((4, 4), 2, &[("Piano", &[(0.0, 1.0, 60), (0.0, 1.0, 60), (2.0, 2.0, 64), (3.0, 1.0, 64)]), ("Kick", &[(0.0, 2.0, 36), (1.0, 1.0, 36)])]);
    let r = lint(&p, &only(&[Check::Overlaps]));
    let m = messages(&r, Kind::SelfOverlap);
    assert_eq!(m, ["Piano C4 is an exact duplicate", "Piano E4 still sounds when the next E4 starts (1 beats of overlap; the first note's note-off can cut the repeat short)"]);
    assert_eq!(r.findings[0].severity, Severity::Error);
    assert_eq!(r.findings[1].severity, Severity::Warning);
}

#[test]
fn controller_pile_ups_name_the_controller_and_the_winner() {
    let mut p = project((4, 4), 4, &[("Lead", &[(0.0, 4.0, 72)])]);
    let t = &mut p.tracks[0];
    t.bends = vec![BendPoint { beat: 4.0, value: 0 }, BendPoint { beat: 4.0, value: 0 }, BendPoint { beat: 8.0, value: 4096 }, BendPoint { beat: 8.0, value: 8191 }];
    t.ccs = vec![CcPoint { beat: 0.0, cc: 1, value: 40 }, CcPoint { beat: 1.0, cc: 1, value: 50 }, CcPoint { beat: 1.0, cc: 11, value: 50 }];
    let r = lint(&p, &only(&[Check::Controllers]));
    assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
    let f = &r.findings[0];
    assert_eq!((f.bar, f.also_bars.clone(), f.severity), (2, vec![3], Severity::Warning));
    assert_eq!(f.message, "Lead: 4 pitch-bend points at the same beat in 2 places (up to 2 at once, 1 with different values: the last one wins; e.g. bar 3 beat 0: 4096, 8191)");
}

#[test]
fn notes_and_data_past_the_end() {
    let mut p = project((4, 4), 2, &[("Pad", &[(0.0, 4.0, 60), (6.0, 30.0, 64), (8.0, 1.0, 67)])]);
    p.tracks[0].ccs = vec![CcPoint { beat: 9.0, cc: 1, value: 3 }];
    p.master.tail_s = 2.0;
    let r = lint(&p, &only(&[Check::PastEnd]));
    let m = messages(&r, Kind::PastEnd);
    assert_eq!(
        m,
        [
            "Pad E4 is cut off by the end of the render (14 s after the song ends; the tail is 2 s)",
            "Pad G4 starts after the end of the song (bar 2): it only plays in the release tail",
            "Pad: 1 controller point after the end of the song",
        ]
    );
    // a looped song folds its tail onto the start: ringing past the loop point is how loops are built
    p.looped = true;
    let r = lint(&p, &only(&[Check::PastEnd]));
    assert_eq!(r.count(Kind::PastEnd), 2);
    assert!(messages(&r, Kind::PastEnd)[0].contains("folds onto the start of the loop"));
}

#[test]
fn keyswitch_hygiene_with_a_card() {
    let mut p = project((4, 4), 2, &[("Strings", &[(0.0, 0.02, 24), (0.1, 2.0, 60), (2.0, 0.02, 62), (4.0, 2.0, 25)])]);
    p.tracks[0].instrument = Instrument::Sfz { path: "sfz/strings.sfz".into(), init_cc: Default::default() };
    let mut card = InstrumentInfo { paths: vec!["sfz/strings.sfz".into()], ..InstrumentInfo::with_range("strings", 36, 96) };
    card.keyswitches = [(24, "sustain".to_string()), (25, "staccato".to_string())].into_iter().collect();
    let opts = LintOptions { catalog: Some(InstrumentCatalog::new(vec![card])), ..LintOptions::default() };
    let r = lint(&p, &opts);
    assert_eq!(
        messages(&r, Kind::Keyswitch),
        [
            "Strings has a 0.02-beat note on D4, which is not a keyswitch of strings (they are C1 (sustain), C#1 (staccato))",
            "Strings holds the keyswitch C#1 (staccato) for 2 beats: it switches the articulation but does not sound"
        ]
    );
    // the held keyswitch is not a heard note: it is neither out of range nor muddy
    assert_eq!(r.count(Kind::Range), 0);
}

#[test]
fn unreadable_and_late_chord_marks() {
    let p = with_chords(project((4, 4), 2, &[]), &[(1, 0.0, "C"), (2, 0.0, "Hmaj7"), (3, 0.0, "F")]);
    let r = lint(&p, &only(&[Check::Harmony]));
    let m = messages(&r, Kind::ChordSymbol);
    assert_eq!(m.len(), 2);
    assert!(m[0].starts_with("the chord mark at bar 2 beat 0 cannot be read: chord \"Hmaj7\""), "{}", m[0]);
    assert_eq!(m[1], "the chord mark \"F\" at bar 3 is after the end of the song");
}

#[test]
fn bar_range_limits_findings_and_unknown_tracks_become_notes() {
    let p = project((4, 4), 4, &[("Flute", &[(0.0, 2.0, 77), (8.0, 2.0, 77)]), ("Horn", &[(0.0, 4.0, 64), (8.0, 4.0, 64)])]);
    let opts = LintOptions { from_bar: Some(2), melody: Some(vec![name("Oboe")]), ..only(&[Check::Clash]) };
    let r = lint(&p, &opts);
    assert_eq!(r.findings.len(), 1);
    assert_eq!((r.findings[0].bar, r.findings[0].also_bars.len()), (3, 0));
    assert_eq!(r.notes, ["melody: no track Oboe"]);
    assert!(r.melody.is_empty() && !r.melody_guessed);
}

#[test]
fn text_report_follows_score_lint_layout() {
    let p = with_chords(
        project((4, 4), 4, &[("Flute", &[(0.0, 2.0, 65), (2.0, 2.0, 64), (8.0, 2.0, 65), (10.0, 2.0, 64)]), ("Horn", &[(0.0, 4.0, 52), (8.0, 4.0, 52)])]),
        &[(1, 0.0, "C")],
    );
    let r = lint(&p, &only(&Check::PYTHON));
    let text = r.to_text(40);
    let want = "\
test: 4/4, 4 bars, 2 pitched tracks
  melody: Flute
  harmony: chord track (1 marks)
  range: 0 cards, 0 of 2 pitched tracks matched
  found: 2 sustained clashes, 2 strong-beat non-chord tones (2 appoggiaturas, 0 clashes), 0 out-of-range notes, 0 muddy spots (counted per bar)
   bar 1  clash Flute F4 against Horn E3: minor 9th, 2.0 beats   (also bars 3)
          nct   Flute F4 on 1 over C: appoggiatura -> E4   (also bars 3)
";
    assert_eq!(text, want);
    let short = r.to_text(1);
    assert!(short.ends_with("  ... 1 more (max)\n"), "{short}");
    let j = r.to_json(1);
    assert_eq!(j["counts"], json!({"clash": 2, "nct_appoggiatura": 2}));
    assert_eq!(j["findings"].as_array().map(Vec::len), Some(1));
    assert_eq!(j["more"], 1);
    assert_eq!(j["findings"][0]["kind"], "clash");
    assert_eq!(j["findings"][0]["also_bars"], json!([3]));
}

#[test]
fn kind_names_match_their_json() {
    for k in [
        Kind::Clash,
        Kind::NctAppoggiatura,
        Kind::NctClash,
        Kind::Range,
        Kind::Mud,
        Kind::ParallelFifths,
        Kind::ParallelOctaves,
        Kind::VoiceCrossing,
        Kind::Leap,
        Kind::SelfOverlap,
        Kind::ControllerPileUp,
        Kind::PastEnd,
        Kind::Keyswitch,
        Kind::ChordSymbol,
    ] {
        assert_eq!(json!(k), json!(k.name()));
        assert!(k.label().len() <= 5, "{k:?}");
    }
}

#[test]
fn bar_spans_text() {
    assert_eq!(bar_spans(&[3, 4, 5, 9, 11, 12]), "3-5, 9, 11-12");
    assert_eq!(bar_spans(&[7]), "7");
}

#[test]
fn handler_parses_validates_and_formats() {
    let p = with_chords(project((4, 4), 4, &[("Flute", &[(0.0, 2.0, 77), (8.0, 2.0, 77)]), ("Horn", &[(0.0, 4.0, 64), (8.0, 4.0, 64)])]), &[(1, 0.0, "C")]);
    let out = lint_handler(&p, &json!({"checks": ["clash"], "max_findings": 5})).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out["counts"], json!({"clash": 2}));
    assert_eq!(out["findings"][0]["severity"], "warning");
    let text = lint_handler(&p, &json!({"format": "text", "checks": ["clash", "nct"]})).unwrap_or_else(|e| panic!("{e}"));
    assert!(text["text"].as_str().is_some_and(|t| t.contains("bar 1  clash")), "{text}");
    // severity filter
    let out = lint_handler(&p, &json!({"min_severity": "warning"})).unwrap_or_else(|e| panic!("{e}"));
    assert!(out["findings"].as_array().is_some_and(|f| f.iter().all(|f| f["severity"] != "info")));
    // transposition by track name, ranges by note names
    let out = lint_handler(&p, &json!({"checks": ["clash", "range"], "transpose": {"Horn": -12}, "ranges": {"Flute": ["C4", "C6"]}})).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out["counts"], json!({}));
    assert_eq!(out["transposed"], json!({"Horn": -12}));
    assert_eq!(out["ranges"]["matched"], json!(["Flute"]));
    assert!(lint_handler(&p, &Value::Null).is_ok());

    let err = |args: Value| lint_handler(&p, &args).expect_err("should fail").to_string();
    assert!(err(json!({"checks": ["clash", "tone"]})).contains("unknown check \"tone\" (the checks are clash, nct"));
    assert!(err(json!({"melody": ["Oboe"]})).contains("no track Oboe (the tracks are #1 \"Flute\", #2 \"Horn\")"));
    assert!(err(json!({"ranges": {"Flute": ["C6", "C4"]}})).contains("lowest key first"));
    assert!(err(json!({"ranges": {"Flute": ["C6"]}})).starts_with("lint: "));
    assert!(err(json!({"from_bar": 3, "to_bar": 2})).contains("to_bar 2 is before from_bar 3"));
    assert!(err(json!({"bend_range": 0})).contains("bend_range"));
    assert!(err(json!({"colour": 1})).contains("unknown field `colour`"));
    assert!(err(json!({"catalog_dir": "Z:/no/such/folder"})).contains("catalog_dir"));
}

#[test]
fn schema_lists_every_check() {
    let s = lint_schema();
    let names: Vec<&str> = s["properties"]["checks"]["items"]["enum"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    assert_eq!(names, Check::ALL.map(Check::name));
    assert_eq!(lint_tool()["name"], "lint");
    for c in Check::ALL {
        assert_eq!(Check::parse(c.name()), Some(c));
    }
}

#[test]
fn blips_are_not_heard() {
    let p = project((4, 4), 2, &[("Flute", &[(0.0, 0.02, 77), (0.0, 2.0, 72)]), ("Horn", &[(0.0, 2.0, 64)])]);
    assert!(lint(&p, &only(&[Check::Clash])).findings.is_empty());
}

#[test]
fn odd_projects_do_not_panic() {
    let mut p = with_chords(project((4, 4), 0, &[("Lead", &[(0.0, 1.0, 72), (-1.0, 0.5, 0), (1e9, 1.0, 127)]), ("Drums", &[(0.0, 0.01, 36)])]), &[(1, 0.0, "C"), (1, 0.0, "C")]);
    p.time_signature = (0, 3);
    p.tempo.clear();
    let r = lint(&p, &LintOptions { bend_range: 48.0, max_leap: 0, from_bar: Some(5), to_bar: Some(1), ..LintOptions::default() });
    assert!(r.notes.iter().any(|n| n.contains("judging the metre as 4/4")), "{:?}", r.notes);
    let _ = r.to_text(0);
    let _ = r.to_json(0);
    let p = project((7, 8), 3, &[("Lead", &[(0.0, 3.5, 72)])]);
    for args in [json!({"checks": "clash"}), json!({"melody": [true]}), json!({"transpose": {"Lead": 99}}), json!({"max_findings": -1}), json!([])] {
        assert!(lint_handler(&p, &args).is_err(), "{args}");
    }
}
