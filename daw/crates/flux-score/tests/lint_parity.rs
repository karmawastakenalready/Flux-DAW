//! Parity with tools/score_lint.py: `tests/data/lint_reference.json` holds two made-up sketches (one in 6/8
//! with a low, muddy passage; one in 4/4 with doubled guitars, keyswitches and pitch bends on its lead), their
//! chord charts as chord marks, and the Python linter's report (made by tests/make/make_lint_reference.py). The
//! ported checks (clash, nct, mud; range has no cards here) must print the same summary and finding lines.

use flux_core::{BendPoint, ChordMark, Instrument, Note, Project, Track};
use flux_score::lint::{Check, LintOptions, lint};
use serde_json::{Value, json};

fn song(v: &Value) -> Project {
    let tracks: Vec<Track> = v["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut track: Track = serde_json::from_value(json!({"id": i + 1, "name": t["name"], "instrument": {"kind": "none"}})).expect("track");
            let path = t["path"].as_str().unwrap_or_default();
            if !path.is_empty() {
                track.instrument = Instrument::Sfz { path: path.into(), init_cc: Default::default() };
            }
            track.notes = t["notes"]
                .as_array()
                .expect("notes")
                .iter()
                .map(|n| Note { start: n[0].as_f64().expect("start"), dur: n[1].as_f64().expect("dur"), pitch: n[2].as_u64().expect("pitch") as u8, vel: n[3].as_u64().expect("vel") as u8 })
                .collect();
            track.bends = t["bends"].as_array().expect("bends").iter().map(|b| BendPoint { beat: b[0].as_f64().expect("beat"), value: b[1].as_i64().expect("value") as i16 }).collect();
            track
        })
        .collect();
    let mut p: Project = serde_json::from_value(json!({
        "name": v["name"], "tempo": [{"bar": 1, "bpm": 100.0}], "time_signature": v["time_signature"], "bars": v["bars"],
    }))
    .expect("project");
    p.tracks = tracks;
    p.harmony = v["harmony"]
        .as_array()
        .expect("harmony")
        .iter()
        .map(|m| ChordMark { bar: m[0].as_u64().expect("bar") as u32, beat: m[1].as_f64().expect("beat"), chord: m[2].as_str().expect("chord").into() })
        .collect();
    p
}

#[test]
fn ported_checks_match_score_lint() {
    let data: Value = serde_json::from_str(include_str!("data/lint_reference.json")).expect("lint_reference.json is valid JSON");
    let songs = data["songs"].as_array().expect("songs");
    assert_eq!(songs.len(), 2);
    for v in songs {
        let p = song(v);
        let report = lint(&p, &LintOptions { checks: Some(Check::PYTHON.to_vec()), ..LintOptions::default() });
        let text = report.to_text(500);
        let ours: Vec<&str> = text.lines().skip_while(|l| !l.starts_with("  found:")).collect();
        let python: Vec<&str> = v["python"].as_array().expect("python lines").iter().map(|l| l.as_str().expect("line")).collect();
        assert!(python.len() > 10, "{}: the reference has findings", p.name);
        for (i, (a, b)) in ours.iter().zip(&python).enumerate() {
            assert_eq!(a, b, "{} line {}", p.name, i + 1);
        }
        assert_eq!(ours.len(), python.len(), "{}: {} lines here, {} from score_lint.py", p.name, ours.len(), python.len());
    }
}
