//! Parity with daw/bridge/score.py: `tests/data/python_reference.json` holds score.py's outputs for a battery
//! of made-up inputs (made by tests/make/make_reference.py). Notation, chord tones, metric weights,
//! voice leading, bass lines, ramps, curves and swells must match exactly; phrasing must match exactly
//! without random variation and structurally with it (score.py uses Python's Mersenne Twister, this crate
//! splitmix64).

use flux_core::Note;
use flux_score::meter::Meter;
use flux_score::notation::{MelodyOptions, melody};
use flux_score::phrase::{PhraseOptions, phrase};
use flux_score::shapes::{Shape, curve, ramp, swell, to_cc};
use flux_score::theory::{Chord, Role};
use flux_score::voicing::{BassLineOptions, VoiceLeadOptions, bass_line, voice_lead};
use serde_json::Value;
use std::sync::OnceLock;

fn reference() -> &'static Value {
    static REF: OnceLock<Value> = OnceLock::new();
    REF.get_or_init(|| {
        let text = include_str!("data/python_reference.json");
        serde_json::from_str(text).expect("python_reference.json is valid JSON")
    })
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

fn u8_of(v: &Value) -> u8 {
    v.as_u64().unwrap_or_else(|| panic!("not an integer: {v}")) as u8
}

fn notes_of(v: &Value) -> Vec<Note> {
    v.as_array()
        .expect("note list")
        .iter()
        .map(|n| Note { start: f(&n["beat"]), dur: f(&n["dur"]), pitch: u8_of(&n["pitch"]), vel: u8_of(&n["vel"]) })
        .collect()
}

fn chords_of(v: &Value) -> Vec<Chord> {
    v.as_array().expect("chord list").iter().map(|c| Chord::parse(c.as_str().expect("symbol")).expect("score.py symbol parses")).collect()
}

fn pitches_of(v: &Value) -> Option<Vec<u8>> {
    v.as_array().map(|a| a.iter().map(u8_of).collect())
}

#[test]
fn metric_weight_matches() {
    for case in reference()["metric_weight"].as_array().unwrap() {
        let meter = Meter::parse(case[0].as_str().unwrap()).unwrap();
        let beat = f(&case[1]);
        assert_eq!(meter.weight(beat), f(&case[2]), "{meter} at beat {beat}");
    }
}

#[test]
fn chord_tones_and_roles_match() {
    let role = |r: &str| match r {
        "root" => Role::Root,
        "third" => Role::Third,
        "fifth" => Role::Fifth,
        "seventh" => Role::Seventh,
        "sus" => Role::Sus,
        "ext" => Role::Extension,
        other => panic!("unknown role {other}"),
    };
    let chords = reference()["chords"].as_object().unwrap();
    assert!(chords.len() > 400);
    for (sym, want) in chords {
        let c = Chord::parse(sym).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.root, u8_of(&want["root"]), "{sym} root");
        assert_eq!(c.bass, u8_of(&want["bass"]), "{sym} bass");
        let pcs: Vec<u8> = c.pcs().collect();
        assert_eq!(pcs, pitches_of(&want["pcs"]).unwrap(), "{sym} tones");
        let roles: Vec<Role> = c.tones.iter().map(|t| t.role).collect();
        let want_roles: Vec<Role> = want["roles"].as_array().unwrap().iter().map(|r| role(r.as_str().unwrap())).collect();
        assert_eq!(roles, want_roles, "{sym} roles");
    }
}

#[test]
fn melody_notation_matches() {
    for case in reference()["melody"].as_array().unwrap() {
        let bpb = f(&case["bpb"]);
        let opts = MelodyOptions {
            unit: f(&case["unit"]),
            vel: f(&case["vel"]),
            transpose: case["shift"].as_i64().unwrap() as i32,
            legato: f(&case["legato"]),
            short: f(&case["short"]),
            arc: f(&case["arc"]),
        };
        let start = (f(&case["bar"]) - 1.0) * bpb;
        let got = melody(case["text"].as_str().unwrap(), start, bpb, &opts).unwrap();
        assert_eq!(got, notes_of(&case["notes"]), "{}", case["text"]);
    }
}

#[test]
fn voice_leading_matches() {
    let cases = reference()["voice_lead"].as_array().unwrap();
    let mut warned = 0;
    for case in cases {
        let chords = chords_of(&case["chords"]);
        let opts = VoiceLeadOptions {
            voices: case["voices"].as_u64().unwrap() as usize,
            low: u8_of(&case["low"]),
            high: u8_of(&case["high"]),
            start: pitches_of(&case["start"]),
            bass: pitches_of(&case["bass"]),
        };
        let name = case["name"].as_str().unwrap();
        let want = &case["result"];
        match voice_lead(&chords, &opts) {
            Ok(vl) => {
                let pitches: Vec<Vec<u8>> = want["pitches"].as_array().unwrap_or_else(|| panic!("{name}: python failed: {want}")).iter().map(|v| pitches_of(v).unwrap()).collect();
                assert_eq!(vl.pitches, pitches, "{name} {opts:?}");
                let warnings: Vec<&str> = want["warnings"].as_array().unwrap().iter().map(|w| w.as_str().unwrap()).collect();
                assert_eq!(vl.warnings, warnings, "{name}");
                warned += warnings.len();
            }
            Err(e) => assert_eq!(want["error"].as_str(), Some(e.to_string().as_str()), "{name}"),
        }
    }
    assert!(cases.len() > 150 && warned > 20, "the battery covers warnings ({warned})");
}

#[test]
fn bass_lines_match() {
    for case in reference()["bass_line"].as_array().unwrap() {
        let chords = chords_of(&case["chords"]);
        let opts = BassLineOptions {
            low: u8_of(&case["low"]),
            high: u8_of(&case["high"]),
            start: case["start"].as_u64().map(|s| s as u8),
            inversions: case["inversions"].as_bool().unwrap(),
        };
        let want = &case["result"];
        match bass_line(&chords, &opts) {
            Ok(b) => assert_eq!(Some(b), pitches_of(&want["ok"]), "{} {opts:?}", case["name"]),
            Err(e) => assert_eq!(want["error"].as_str(), Some(e.to_string().as_str()), "{} {opts:?}", case["name"]),
        }
    }
}

fn phrase_options(o: &Value) -> PhraseOptions {
    let d = PhraseOptions::default();
    let num = |k: &str, dflt: f64| o.get(k).map_or(dflt, f);
    PhraseOptions {
        accent: num("accent", d.accent),
        arc: num("arc", d.arc),
        soften: num("soften", d.soften),
        rit: num("rit", d.rit),
        breath: num("breath", d.breath),
        vary: num("vary", d.vary),
        seed: o.get("seed").and_then(Value::as_u64).unwrap_or(0),
        rest: num("rest", d.rest),
        long: o.get("long").map(f),
        breaks: o["breaks"].as_array().unwrap().iter().map(|b| b.as_u64().unwrap() as u32).collect(),
    }
}

#[test]
fn phrasing_matches() {
    let mut exact = 0;
    for case in reference()["phrase"].as_array().unwrap() {
        let meter = Meter::parse(case["meter"].as_str().unwrap()).unwrap();
        let opts = phrase_options(&case["opts"]);
        let got = phrase(&notes_of(&case["notes"]), meter, &opts);
        let want = notes_of(&case["result"]);
        if opts.vary == 0.0 {
            assert_eq!(got, want, "{meter} {opts:?}");
            exact += 1;
        } else {
            // the same line and timing; velocities differ only by the jitter
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(&want) {
                assert_eq!((g.start, g.dur, g.pitch), (w.start, w.dur, w.pitch), "{meter} {opts:?}");
                assert!((g.vel as f64 - w.vel as f64).abs() <= 2.0 * opts.vary + 1.0, "{meter} {opts:?}: {g:?} vs {w:?}");
            }
        }
    }
    assert!(exact >= 40);
}

fn cc_pairs(v: &Value) -> Vec<(f64, u8)> {
    v.as_array().unwrap().iter().map(|p| (f(&p["beat"]), u8_of(&p["value"]))).collect()
}

#[test]
fn swells_ramps_and_curves_match() {
    for case in reference()["swell"].as_array().unwrap() {
        let o = &case["opts"];
        let pts = swell(&notes_of(&case["notes"]), f(&o["lo"]), f(&o["hi"]), f(&o["long"]), f(&o["step"]), f(&o["rest"]));
        let got: Vec<(f64, u8)> = to_cc(&pts, 1).iter().map(|c| (c.beat, c.value)).collect();
        assert_eq!(got, cc_pairs(&case["result"]), "swell {o}");
    }
    for case in reference()["ramp"].as_array().unwrap() {
        let a: Vec<f64> = case["args"].as_array().unwrap().iter().map(f).collect();
        let got: Vec<(f64, u8)> = to_cc(&ramp(a[0], a[1], a[2], a[3], a[4]), 1).iter().map(|c| (c.beat, c.value)).collect();
        assert_eq!(got, cc_pairs(&case["result"]), "ramp {a:?}");
    }
    for case in reference()["curve"].as_array().unwrap() {
        let pts: Vec<(f64, f64)> = case["points"].as_array().unwrap().iter().map(|p| (f(&p[0]), f(&p[1]))).collect();
        let got: Vec<(f64, u8)> = to_cc(&curve(&pts, 0.5, Shape::Linear), 1).iter().map(|c| (c.beat, c.value)).collect();
        assert_eq!(got, cc_pairs(&case["result"]), "curve {pts:?}");
    }
}
