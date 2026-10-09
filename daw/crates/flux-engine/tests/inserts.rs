//! Insert effects: a VST3 effect on a track, on the reverb return (in place of the built-in reverb) and on the
//! master, rendered offline and played live. Needs Dexed in `plugins/`, Valhalla Supermassive installed and the
//! plugin host built (`cargo build -p flux-vst3`); skipped otherwise.

use flux_core::{Command, Project, commands};
use flux_engine::live::{LiveEngine, RenderHints};
use flux_engine::render::{self, RenderOptions, RenderOutput};
use flux_engine::paths;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEXED: &str = "plugins/dexed/Dexed.vst3";

fn supermassive() -> Option<PathBuf> {
    flux_vst3::scan::standard_dirs().into_iter().map(|d| d.join("ValhallaSupermassive.vst3")).find(|p| p.exists())
}

fn available() -> bool {
    let host = std::env::current_exe().unwrap().ancestors().skip(1).take(2).any(|d| d.join("flux-plugin-host.exe").is_file());
    let ok = host && paths::resolve(DEXED).exists() && supermassive().is_some();
    if !ok {
        eprintln!("skipped: needs {DEXED}, Valhalla Supermassive and flux-plugin-host.exe");
    }
    ok
}

fn verb() -> String {
    supermassive().unwrap().to_string_lossy().into_owned()
}

/// A long, fully wet reverb: what comes out after the notes stop is the insert's.
fn wet() -> Value {
    json!({"Mix": {"normalized": 1.0}, "Feedback": {"normalized": 0.85}, "Delay_Ms": {"normalized": 0.3}})
}

/// Two bars at 120 BPM (4 s): a short chord at the start, then nothing. `extra` commands are applied after.
fn project(extra: Value) -> Project {
    let mut p = Project::new("inserts", 120.0, 2);
    let mut cmds = vec![
        json!({"op": "add_track", "name": "Keys", "instrument": {"kind": "vst3", "path": DEXED}}),
        json!({"op": "add_notes", "track": "Keys", "notes": [
            {"bar": 1, "beat": 0, "dur": 0.5, "pitch": "C4"}, {"bar": 1, "beat": 0, "dur": 0.5, "pitch": "G4"}]}),
        json!({"op": "set_master", "master": {"tail_s": 0.0, "glue": null}}),
    ];
    cmds.extend(extra.as_array().cloned().unwrap_or_default());
    for c in serde_json::from_value::<Vec<Command>>(Value::Array(cmds)).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn rms_db(x: &[f32]) -> f64 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).max(1e-20).log10()
}

fn render(p: &Project) -> RenderOutput {
    render::render(p, &RenderOptions { dry: true, ..RenderOptions::default() }).unwrap()
}

/// How far the level 2.5-3.5 s in (the chord ended at 0.25 s) is below the chord's (dB).
fn tail_below_body(o: &RenderOutput) -> f64 {
    let sr = o.sr as usize;
    rms_db(&o.left[..sr / 4]) - rms_db(&o.left[sr * 5 / 2..sr * 7 / 2])
}

#[test]
fn a_track_insert_processes_the_track_and_bypass_skips_it() {
    if !available() {
        return;
    }
    let add = |bypass: bool| json!([{"op": "add_insert", "track": "Keys", "path": verb(), "params": wet(), "bypass": bypass}]);
    let on = render(&project(add(false)));
    assert!(on.tracks[0].warning.is_none(), "{:?}", on.tracks[0].warning);
    assert!(on.warnings.is_empty(), "{:?}", on.warnings);
    let off = render(&project(add(true)));
    let (t_on, t_off) = (tail_below_body(&on), tail_below_body(&off));
    assert!(t_on < 30.0, "the reverb should ring on after the chord ({t_on:.1} dB below it)");
    assert!(t_off > t_on + 20.0, "bypassed, nothing should ring ({t_off:.1} vs {t_on:.1} dB below the chord)");

    // a pre-strip insert still processes the track
    let pre = render(&project(json!([{"op": "add_insert", "track": "Keys", "path": verb(), "params": wet(), "position": "pre"}])));
    assert!(tail_below_body(&pre) < 30.0, "a pre insert should process the track too");
}

#[test]
fn the_reverb_return_runs_through_its_inserts_in_place_of_the_built_in_reverb() {
    if !available() {
        return;
    }
    let p = |bypass: bool| {
        project(json!([
            {"op": "set_mixer", "track": "Keys", "mixer": {"reverb_send": 1.0}},
            {"op": "add_insert", "bus": "reverb", "path": verb(), "params": wet(), "bypass": bypass}
        ]))
    };
    let on = render(&p(false));
    assert!(on.warnings.is_empty(), "{:?}", on.warnings);
    let dry = on.dry.as_ref().expect("asked for the dry master");
    let sr = on.sr as usize;
    let wet_tail = rms_db(&on.left[sr * 5 / 2..sr * 7 / 2]);
    let dry_tail = rms_db(&dry.0[sr * 5 / 2..sr * 7 / 2]);
    assert!(wet_tail > dry_tail + 20.0, "the return's reverb should ring ({wet_tail:.1} vs dry {dry_tail:.1} dBFS)");

    // bypassed, the sends pass straight through: no reverb at all, not the built-in one either
    let off = render(&p(true));
    assert!(tail_below_body(&off) > tail_below_body(&on) + 20.0, "a bypassed return chain mustn't fall back to the built-in reverb");
}

#[test]
fn a_master_insert_that_fails_passes_audio_through_with_a_warning() {
    if !available() {
        return;
    }
    let o = render(&project(json!([{"op": "add_insert", "bus": "master", "path": "plugins/no-such-effect.vst3"}])));
    assert!(o.warnings.iter().any(|w| w.contains("no-such-effect") && w.contains("passed through")), "{:?}", o.warnings);
    assert!(rms_db(&o.left[..o.sr as usize / 4]) > -40.0, "the mix still plays");

    let ok = render(&project(json!([{"op": "add_insert", "bus": "master", "path": verb(), "params": wet()}])));
    assert!(ok.warnings.is_empty(), "{:?}", ok.warnings);
    assert!(tail_below_body(&ok) < 30.0, "the master insert should process the mix");
}

#[test]
fn a_looped_song_with_a_long_reverb_insert_still_loops_seamlessly() {
    if !available() {
        return;
    }
    // Supermassive reports no tail: Flux measures it (about 20 s here) and leads in that long, so the loop starts
    // where continuous playback would be
    let still = json!({"Mix": {"normalized": 1.0}, "Feedback": {"normalized": 0.85}, "Delay_Ms": {"normalized": 0.3}, "ModDepth": {"normalized": 0.0}});
    let p = project(json!([
        {"op": "set_project", "looped": true},
        {"op": "add_insert", "track": "Keys", "path": verb(), "params": still}
    ]));
    let tail = flux_engine::vst::insert_tail_s(&p, p.tracks[0].mixer.inserts[0].id).unwrap();
    assert!(tail.is_some_and(|t| t > 10.0 && t < 60.0), "measured tail {tail:?}");
    let o = render(&p);
    let seam = o.seam_error_db.expect("a looped render measures its seam");
    assert!(seam < -90.0, "loop seam error {seam:.1} dB");
    assert_eq!(o.seam_crossfade_ms, None);
    // the tail from the end of the loop rings into its start
    let sr = o.sr as usize;
    assert!(rms_db(&o.left[sr * 7 / 2..]) > -50.0, "the reverb rings across the loop point");

    // with its modulation running free the reverb never repeats exactly: clean anyway, or crossfaded at the seam
    let p = project(json!([
        {"op": "set_project", "looped": true},
        {"op": "add_insert", "track": "Keys", "path": verb(), "params": wet()}
    ]));
    let o = render(&p);
    let seam = o.seam_error_db.unwrap();
    assert!(seam < -60.0 || o.seam_crossfade_ms.is_some(), "loop seam error {seam:.1} dB and no crossfade");
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
    assert_eq!(e.plugins_loading(), 0, "the plugins didn't load: {}", status(e));
}

/// `secs` of live output, left channel.
fn play(e: &mut LiveEngine, secs: f64) -> Vec<f32> {
    let n = (secs * e.sample_rate() as f64) as usize;
    let (mut l, mut r, mut pos) = (vec![0f32; n], vec![0f32; n], vec![0u32; n]);
    assert_eq!(e.render(&mut l, &mut r, &mut pos), n);
    l
}

#[test]
fn live_playback_runs_track_and_bus_inserts() {
    if !available() {
        return;
    }
    let p = |bypass: bool| {
        Arc::new(project(json!([
            {"op": "add_insert", "track": "Keys", "path": verb(), "params": wet(), "bypass": bypass},
            {"op": "add_insert", "bus": "master", "path": verb(), "params": {"Mix": {"normalized": 0.2}}}
        ])))
    };
    let hints = RenderHints { drive_db: Some(0.0), ..RenderHints::default() };
    let mut e = LiveEngine::new(p(false), hints);
    settle(&mut e);
    let out = play(&mut e, 3.5);
    assert!(!status(&e).contains("insert"), "inserts should be live: {}", status(&e));
    let sr = e.sample_rate() as usize;
    let on = rms_db(&out[..sr / 4]) - rms_db(&out[sr * 5 / 2..]);
    assert!(on < 30.0, "the track's reverb insert should ring live ({on:.1} dB below the chord)");

    // bypassing it is heard at once, without reloading anything
    e.update(p(true));
    assert_eq!(e.plugins_loading(), 0, "bypass mustn't reload plugins");
    let t0 = Instant::now();
    let mut off = 0.0;
    while t0.elapsed() < Duration::from_secs(10) {
        e.seek(0);
        let out = play(&mut e, 3.5);
        off = rms_db(&out[..sr / 4]) - rms_db(&out[sr * 5 / 2..]);
        if off > on + 15.0 {
            break;
        }
    }
    assert!(off > on + 15.0, "bypassed live, the track shouldn't ring ({off:.1} vs {on:.1} dB)");
}
