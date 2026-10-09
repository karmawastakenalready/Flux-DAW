//! Rigs (`.rig.json`): an SFZ source through flux-nam's insert chain. Needs METAL-GTX and the NAM captures
//! (`flux assets fetch`); skipped otherwise.

use flux_core::{Command, Project, TimeMap, commands};
use flux_engine::{paths, render, rig};
use serde_json::{Value, json};

const RIG: &str = "sfz/rigs/gtx_5150_L.rig.json";

fn project(params: Value) -> Project {
    let mut p = Project::new("rig", 120.0, 2);
    let cmds = json!([
        {"op": "add_track", "name": "Gtr", "instrument": {"kind": "vst3", "path": RIG, "params": params}},
        {"op": "add_notes", "track": "Gtr", "notes": [{"bar": 1, "beat": 1, "dur": 1.0, "pitch": "E2"}, {"bar": 1, "beat": 1, "dur": 1.0, "pitch": "B2"}]}
    ]);
    for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn available() -> bool {
    let ok = paths::sfizz_dll().is_file() && rig::load(&project(json!({})).tracks[0]).is_ok();
    if !ok {
        eprintln!("skipped: needs sfizz and the {RIG} rig's SFZ and NAM model");
    }
    ok
}

fn rms_db(x: &[f32]) -> f64 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).max(1e-20).log10()
}

#[test]
fn a_rig_sounds_on_time_and_takes_track_params() {
    if !available() {
        return;
    }
    let p = project(json!({}));
    let (tm, t) = (TimeMap::new(&p), &p.tracks[0]);
    let sr = p.sample_rate as usize;
    let frames = 2 * sr;
    let ((l, r), note) = render::render_instrument_here(t, &p, &tm, frames).unwrap();
    assert_eq!(note, "rig: gtx_5150_L (sfizz > highpass > nam)");
    assert_eq!((l.len(), r.len()), (frames, frames));
    assert_eq!(l, r, "the chain is mono");

    // the chord starts at beat 1 (0.5 s): silence before it (the chain's latency is compensated), sound after
    let onset = sr / 2;
    let before = rms_db(&l[onset - sr / 5..onset - sr / 50]);
    let after = rms_db(&l[onset..onset + sr / 5]);
    assert!(after > -40.0, "the rig should sound ({after:.1} dBFS)");
    // (a high-gain amp hisses at about -60 dBFS on its own)
    assert!(before < after - 30.0, "sound before the note: {before:.1} vs {after:.1} dBFS");
    let first = l.iter().position(|x| x.abs() > 1e-2).unwrap();
    assert!(first.abs_diff(onset) < sr / 100, "the note starts at {first}, expected about {onset}");

    // track params override stage settings: the amp's output 6 dB down
    let quieter = project(json!({"nam.output_db": -12.0}));
    let (q, _) = render::render_instrument_here(&quieter.tracks[0], &quieter, &tm, frames).unwrap().0;
    let drop = after - rms_db(&q[onset..onset + sr / 5]);
    assert!((drop - 6.0).abs() < 0.5, "nam.output_db -12 (from -6) should be 6 dB quieter, got {drop:.2} dB");

    let bad = project(json!({"amp.gain": 3}));
    let e = format!("{:#}", render::render_instrument_here(&bad.tracks[0], &bad, &tm, frames).unwrap_err());
    assert!(e.contains("amp.gain"), "{e}");
}
