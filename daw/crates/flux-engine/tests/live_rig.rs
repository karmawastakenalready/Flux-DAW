//! Live playback of a rig: it loads, plays in time and at the level of the offline render, takes a change of
//! its params while playing without reloading, and picks up held notes after a seek. Needs METAL-GTX and the
//! NAM captures (`flux assets fetch`); skipped otherwise.

use flux_core::{Command, Project, commands};
use flux_engine::live::{LiveEngine, RenderHints};
use flux_engine::{RenderOptions, paths, rig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RIG: &str = "sfz/rigs/gtx_5150_L.rig.json";

fn project(params: Value) -> Project {
    let mut p = Project::new("live-rig", 120.0, 2);
    let cmds = json!([
        {"op": "add_track", "name": "Gtr", "instrument": {"kind": "vst3", "path": RIG, "params": params}},
        {"op": "add_notes", "track": "Gtr", "notes": [
            {"bar": 1, "beat": 1, "dur": 1.0, "pitch": "E2"}, {"bar": 1, "beat": 1, "dur": 1.0, "pitch": "B2"},
            {"bar": 2, "beat": 0, "dur": 3.0, "pitch": "E2"}, {"bar": 2, "beat": 0, "dur": 3.0, "pitch": "B2"}]}
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

fn status(e: &LiveEngine) -> String {
    e.status().into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("; ")
}

fn settle(e: &mut LiveEngine) {
    let t0 = Instant::now();
    while (e.loading() > 0 || e.plugins_loading() > 0) && t0.elapsed() < Duration::from_secs(120) {
        e.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(status(e), "live", "the rig didn't load");
}

fn play(e: &mut LiveEngine, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r, mut pos) = (vec![0f32; frames], vec![0f32; frames], vec![0u32; frames]);
    assert_eq!(e.render(&mut l, &mut r, &mut pos), frames);
    (l, r)
}

fn rms_db(x: &[f32]) -> f64 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).max(1e-20).log10()
}

#[test]
fn a_rig_plays_live_like_its_render_and_follows_edits() {
    if !available() {
        return;
    }
    let p = project(json!({}));
    let sr = p.sample_rate as usize;
    let out = flux_engine::render(&p, &RenderOptions::default()).unwrap();
    let mut e = LiveEngine::new(Arc::new(p), RenderHints::from_render(&out));
    let t0 = Instant::now();
    settle(&mut e);
    eprintln!("rig loaded live in {:.1} s", t0.elapsed().as_secs_f64());

    // the whole song from the top: on time and at the render's level (the chain's latency is compensated)
    e.seek(0);
    let (l, _) = play(&mut e, 4 * sr);
    let onset = |x: &[f32]| x.iter().position(|v| v.abs() > 1e-2).unwrap_or(usize::MAX);
    let (live_on, render_on) = (onset(&l), onset(&out.left));
    assert!(live_on.abs_diff(sr / 2) < sr / 100, "the first chord should start at 0.5 s, live it starts at {live_on}");
    assert!(live_on.abs_diff(render_on) <= sr / 1000, "live onset {live_on} vs render {render_on}");
    let span = sr / 2..sr * 3;
    let (live_db, render_db) = (rms_db(&l[span.clone()]), rms_db(&out.left[span]));
    assert!((live_db - render_db).abs() < 1.5, "live {live_db:.1} dBFS vs render {render_db:.1} dBFS");
    assert!(status(&e) == "live", "{}", status(&e));

    // a param change while playing builds the chain again; the source keeps playing (no reload)
    e.seek(2 * sr);
    let loud = rms_db(&play(&mut e, sr / 2).0);
    let t1 = Instant::now();
    e.update(Arc::new(project(json!({"nam.output_db": -12.0}))));
    assert_eq!((e.loading(), e.plugins_loading()), (0, 0), "a param change mustn't reload the rig: {}", status(&e));
    let mut quiet = None;
    while t1.elapsed() < Duration::from_secs(10) {
        e.seek(2 * sr);
        let db = rms_db(&play(&mut e, sr / 2).0);
        if db < loud - 3.0 {
            quiet = Some(db);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let quiet = quiet.unwrap_or_else(|| panic!("the amp 6 dB down should be heard live ({loud:.1} dBFS before): {}", status(&e)));
    eprintln!("param change heard after {:.2} s: {loud:.1} -> {quiet:.1} dBFS", t1.elapsed().as_secs_f64());
    assert!(t1.elapsed() < Duration::from_secs(3), "rebuilding the chain took {:.1} s", t1.elapsed().as_secs_f64());
    assert!(quiet > loud - 8.0, "{loud:.1} -> {quiet:.1} dBFS for 6 dB less from the amp");
    assert_eq!(status(&e), "live");

    // a seek into the held chord restarts it
    e.seek(2 * sr + sr / 4);
    let held = rms_db(&play(&mut e, sr / 4).0);
    assert!(held > quiet - 10.0, "the chord held across the seek point should sound ({held:.1} dBFS)");
}
