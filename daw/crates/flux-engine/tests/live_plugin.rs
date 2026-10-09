//! Live playback of a VST3 instrument through its own realtime instance: loading, a parameter change
//! while playing, a seek, and unloading while idle. Needs Dexed in `plugins/` and the plugin host built
//! (`cargo build -p flux-vst3`); skipped otherwise.

use flux_core::{Command, Project, commands};
use flux_engine::live::{LiveEngine, RenderHints};
use flux_engine::paths;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEXED: &str = "plugins/dexed/Dexed.vst3";

fn project(output: Option<f64>) -> Project {
    let mut p = Project::new("live-plugin", 120.0, 2);
    let mut instrument = json!({"kind": "vst3", "path": DEXED});
    if let Some(o) = output {
        instrument["params"] = json!({"Output": {"normalized": o}});
    }
    let cmds = json!([
        {"op": "add_track", "name": "Keys", "instrument": instrument},
        {"op": "add_notes", "track": "Keys", "notes": [
            {"bar": 1, "beat": 0, "dur": 3.5, "pitch": "C4"}, {"bar": 1, "beat": 0, "dur": 3.5, "pitch": "G4"},
            {"bar": 2, "beat": 0, "dur": 3.5, "pitch": "A3"}, {"bar": 2, "beat": 0, "dur": 3.5, "pitch": "E4"}]}
    ]);
    for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn available() -> bool {
    let host = std::env::current_exe().unwrap().ancestors().skip(1).take(2).any(|d| d.join("flux-plugin-host.exe").is_file());
    let ok = host && paths::resolve(DEXED).exists();
    if !ok {
        eprintln!("skipped: needs {DEXED} and flux-plugin-host.exe");
    }
    ok
}

fn status(e: &LiveEngine) -> String {
    e.status().into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("; ")
}

fn settle(e: &mut LiveEngine) {
    let t0 = Instant::now();
    while (e.loading() > 0 || e.plugins_loading() > 0) && t0.elapsed() < Duration::from_secs(60) {
        e.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(e.plugins_loading(), 0, "the plugin didn't load: {}", status(e));
}

/// RMS (dBFS) of `secs` of output.
fn play(e: &mut LiveEngine, secs: f64) -> f64 {
    let n = (secs * e.sample_rate() as f64) as usize;
    let (mut l, mut r, mut pos) = (vec![0f32; n], vec![0f32; n], vec![0u32; n]);
    let got = e.render(&mut l, &mut r, &mut pos);
    assert_eq!(got, n);
    let ms = l.iter().chain(&r).map(|x| (*x as f64).powi(2)).sum::<f64>() / (2 * n) as f64;
    10.0 * ms.max(1e-20).log10()
}

/// Play the start again until the level meets `ok` twice in a row within 1 dB (set-up changes land in
/// the background).
fn level_after_change(e: &mut LiveEngine, ok: impl Fn(f64) -> bool) -> Option<f64> {
    let t0 = Instant::now();
    let mut last: Option<f64> = None;
    while t0.elapsed() < Duration::from_secs(10) {
        e.seek(0);
        let l = play(e, 1.5);
        if ok(l) && last.is_some_and(|p| (p - l).abs() < 1.0) {
            return Some(l);
        }
        last = Some(l);
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn a_live_plugin_plays_follows_edits_and_unloads_when_idle() {
    if !available() {
        return;
    }
    // no render hints: a fixed drive, so levels compare across the steps
    let hints = RenderHints { drive_db: Some(0.0), ..RenderHints::default() };
    let mut e = LiveEngine::new(Arc::new(project(None)), hints);
    settle(&mut e);
    assert_eq!(status(&e), "live");
    let loud = play(&mut e, 2.0);
    assert!(loud > -40.0, "the plugin should sound ({loud:.1} dBFS)");
    assert_eq!(status(&e), "live", "no late blocks while playing");

    // the same plugin with a parameter changed: set up again as it plays, not reloaded
    e.update(Arc::new(project(Some(0.2))));
    assert_eq!(e.plugins_loading(), 0, "a parameter change mustn't reload the plugin");
    let quiet = level_after_change(&mut e, |l| l < loud - 6.0);
    let quiet = quiet.unwrap_or_else(|| panic!("turning Output down should be heard live (from {loud:.1} dBFS)"));
    assert_eq!(status(&e), "live");

    // ...without cutting the notes it is playing: only the changed parameter is sent
    e.seek(0);
    let mut levels = vec![play(&mut e, 0.25)];
    e.update(Arc::new(project(Some(0.5))));
    for _ in 0..25 {
        levels.push(play(&mut e, 0.05));
        std::thread::sleep(Duration::from_millis(10));
    }
    // (the patch has a tremolo: chunks swing by ~15 dB; a cut note would be ~-100 dBFS)
    let floor = levels.iter().cloned().fold(f64::INFINITY, f64::min);
    assert!(floor > quiet - 12.0, "a parameter change cut the sounding notes: {levels:.1?} (steady level {quiet:.1})");
    let quiet = level_after_change(&mut e, |l| l > quiet + 3.0).expect("Output 0.5 is applied");

    // a seek lands on the position asked for, and the plugin's notes are restarted there
    e.seek(e.sample_rate() as usize);
    assert_eq!(e.position(), e.sample_rate() as usize);
    assert!(play(&mut e, 0.5) > -60.0, "the chord held across the seek point should sound");

    // idle: the plugin is unloaded, then loaded again for playback
    assert_eq!(e.release_plugins(), 1);
    assert!(status(&e).starts_with("standby"), "{}", status(&e));
    e.wake();
    settle(&mut e);
    assert_eq!(status(&e), "live");
    e.seek(0);
    let again = play(&mut e, 1.5);
    assert!((again - quiet).abs() < 3.0, "reloaded, it is set up from the project again ({quiet:.1} vs {again:.1} dBFS)");
}
