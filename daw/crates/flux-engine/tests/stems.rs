//! Track stems and the dry master (`RenderOptions::track_stems`, `RenderOptions::dry`): the stems sum to the master
//! through every master stage, the dry master lines up with it but has no reverb or delay, and layer stems still
//! sum to the master when everything is asked for at once. Audio tracks only, so no instruments are needed.

use flux_core::{Command, Project, commands};
use flux_engine::dsp::Stereo;
use flux_engine::render::write_wav_f32;
use flux_engine::{RenderOptions, RenderOutput, render};
use serde_json::{Value, json};
use std::path::PathBuf;

const SR: u32 = 48_000;

/// Short decaying bursts at the given times (seconds), a different pitch per part.
fn bursts(times: &[f64], hz: f64, len_s: f64) -> Stereo {
    let n = (len_s * SR as f64) as usize;
    let (mut l, mut r) = (vec![0f32; n], vec![0f32; n]);
    for &t in times {
        let s = (t * SR as f64) as usize;
        for k in 0..(SR as usize / 20).min(n.saturating_sub(s)) {
            let x = k as f64 / SR as f64;
            let v = (0.4 * (-x * 60.0).exp() * (std::f64::consts::TAU * hz * x).sin()) as f32;
            l[s + k] += v;
            r[s + k] += 0.8 * v;
        }
    }
    (l, r)
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("flux-stems-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn project(looped: bool) -> Project {
    let d = dir();
    let len = 8.0; // 4 bars of 4/4 at 120 bpm
    let parts = [("kick", 60.0, vec![0.0, 2.0, 4.0, 6.0]), ("snare", 220.0, vec![1.0, 3.0, 5.0, 7.0]), ("hat", 3000.0, vec![0.5, 1.5, 6.5])];
    let mut cmds = vec![json!({"op": "set_project", "bars": 4, "looped": looped})];
    for (name, hz, times) in &parts {
        let f = d.join(format!("{name}.wav"));
        write_wav_f32(&f, SR, &bursts(times, *hz, len)).unwrap();
        cmds.push(json!({"op": "add_track", "name": name, "instrument": {"kind": "audio", "path": f.to_string_lossy()}}));
    }
    cmds.push(json!({"op": "set_mixer", "track": "kick", "mixer": {"reverb_send": 0.1}}));
    cmds.push(json!({"op": "set_mixer", "track": "snare", "mixer": {"reverb_send": 0.6, "pan": -0.4}}));
    cmds.push(json!({"op": "set_mixer", "track": "hat", "mixer": {"delay": {"seconds": 0.375, "feedback": 0.4, "mix": 0.5}, "pan": 0.5}}));
    cmds.push(json!({"op": "set_master", "master": {"glue": {"threshold_db": -6.0, "relative": true, "ratio": 3.0},
        "eq": [{"kind": "low_shelf", "hz": 120.0, "db": 2.0}]}}));
    let mut p = Project::new("stems-test", 120.0, 4);
    p.sample_rate = SR;
    for c in serde_json::from_value::<Vec<Command>>(Value::Array(cmds)).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn max_diff_db(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len());
    let d = a.iter().zip(b).fold(0f32, |m, (x, y)| m.max((x - y).abs()));
    20.0 * (d as f64 + 1e-30).log10()
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|v| (*v as f64).powi(2)).sum()
}

fn check(out: &RenderOutput) {
    let n = out.left.len();
    assert_eq!(out.track_stems.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["kick", "snare", "hat"]);
    let (mut sl, mut sr) = (vec![0f32; n], vec![0f32; n]);
    for (name, (l, r)) in &out.track_stems {
        assert_eq!((l.len(), r.len()), (n, n), "{name} stem length");
        for k in 0..n {
            sl[k] += l[k];
            sr[k] += r[k];
        }
    }
    let err = max_diff_db(&sl, &out.left).max(max_diff_db(&sr, &out.right));
    assert!(err < -90.0, "the track stems should sum to the master, off by {err:.1} dBFS");
    assert!(out.layer_sum_error_db.unwrap() < -90.0, "layers off by {:?}", out.layer_sum_error_db);

    // the dry master: same length, no reverb tail after the snare, no delay echoes after the last hat
    let (dl, _) = out.dry.as_ref().expect("dry master");
    assert_eq!(dl.len(), n);
    let win = |t: f64| (t * SR as f64) as usize..((t + 0.3) * SR as f64) as usize;
    // 0.1 s after a snare hit: the burst is over, the reverb rings in the master only
    let tail = win(7.1);
    assert!(energy(&out.left[tail.clone()]) > 20.0 * energy(&dl[tail]), "the dry master should have no reverb tail");
    // 6.5 s hat + 0.375 s delay: an echo in the master, nothing dry (the kick at 6.0 has decayed by then)
    let echo = (6.875 * SR as f64) as usize..(6.95 * SR as f64) as usize;
    assert!(energy(&out.left[echo.clone()]) > 10.0 * energy(&dl[echo]), "the dry master should have no delay echoes");
    // during the hits the two line up (same master gains)
    let hit = (2.0 * SR as f64) as usize..(2.02 * SR as f64) as usize;
    let ratio = 10.0 * (energy(&out.left[hit.clone()]) / energy(&dl[hit])).log10();
    assert!(ratio.abs() < 1.5, "kick level wet vs dry differs by {ratio:.1} dB");
}

#[test]
fn track_stems_and_the_dry_master_line_up_with_the_master() {
    for looped in [true, false] {
        let p = project(looped);
        let all = RenderOptions { layers: true, track_stems: true, dry: true, ..Default::default() };
        let out = render(&p, &all).unwrap();
        check(&out);
        // asking for stems doesn't change the master
        let plain = render(&p, &RenderOptions::default()).unwrap();
        assert!(plain.track_stems.is_empty() && plain.dry.is_none());
        assert_eq!(plain.left.len(), out.left.len());
        let err = max_diff_db(&plain.left, &out.left);
        assert!(err < -100.0, "looped={looped}: the master changed by {err:.1} dBFS when stems were kept");
    }
}

#[test]
fn muted_tracks_have_no_stem() {
    let mut p = project(true);
    let cmd: Command = serde_json::from_value(json!({"op": "set_mixer", "track": "hat", "mixer": {"mute": true}})).unwrap();
    commands::apply(&mut p, &cmd).unwrap();
    let out = render(&p, &RenderOptions { track_stems: true, ..Default::default() }).unwrap();
    assert_eq!(out.track_stems.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["kick", "snare"]);
}
