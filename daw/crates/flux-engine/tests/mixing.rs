//! Group buses, aux sends and the master chain (glue, EQ in minimum and linear phase, dynamic EQ, de-esser, multiband,
//! exciter, imager, image, the true-peak limiter) in live playback, against the offline render. Audio tracks only, so
//! no instruments are needed.

use flux_core::{Command, Project, commands};
use flux_engine::dsp::Stereo;
use flux_engine::live::{LiveEngine, RenderHints};
use flux_engine::render::write_wav_f32;
use flux_engine::{RenderOptions, render};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SR: u32 = 48_000;

fn s(t: f64) -> usize {
    (t * SR as f64) as usize
}

/// A steady tone, a little wider on the right.
fn tone(hz: f64, len_s: f64) -> Stereo {
    let w = |k: usize, a: f64| (a * (std::f64::consts::TAU * hz * k as f64 / SR as f64).sin()) as f32;
    ((0..s(len_s)).map(|k| w(k, 0.15)).collect(), (0..s(len_s)).map(|k| w(k, 0.2)).collect())
}

/// Short bright bursts every half second.
fn bursts(len_s: f64) -> Stereo {
    let mut l = vec![0f32; s(len_s)];
    let mut t = 0.25;
    while t + 0.1 < len_s {
        for k in 0..s(0.08) {
            let x = k as f64 / SR as f64;
            l[s(t) + k] += (0.5 * (-x * 40.0).exp() * (std::f64::consts::TAU * 3000.0 * x).sin()) as f32;
        }
        t += 0.5;
    }
    (l.clone(), l)
}

fn wav(a: &Stereo) -> String {
    // the tests run at once: a file each
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("flux-mixing-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let f = d.join(format!("{}.wav", N.fetch_add(1, Ordering::Relaxed)));
    write_wav_f32(&f, SR, a).unwrap();
    f.to_string_lossy().into_owned()
}

fn reverb(algorithm: &str, mix: f64) -> Value {
    json!({"room_size": 0.6, "damping": 0.4, "predelay_ms": 12.0, "hpf_hz": 150.0, "lpf_hz": 9000.0, "return_db": 0.0,
        "algorithm": algorithm, "decay_s": 1.6, "mix": mix})
}

/// Two tracks into a group bus that compresses (with lookahead), tames a band and de-esses them, both sending to
/// a reverb return; the master glues (with lookahead), compresses in bands, folds the lows to mono and widens.
fn project(looped: bool) -> Project {
    let cmds = vec![
        json!({"op": "set_project", "bars": 4, "looped": looped}),
        json!({"op": "add_track", "name": "pad", "instrument": {"kind": "audio", "path": wav(&tone(220.0, 8.0))}}),
        json!({"op": "add_track", "name": "hits", "instrument": {"kind": "audio", "path": wav(&bursts(8.0))}}),
        json!({"op": "set_mixer", "track": "hits", "mixer": {"comp": {"threshold_db": -6.0, "relative": true, "ratio": 3.0,
            "attack_ms": 2.0, "release_ms": 80.0, "lookahead_ms": 3.0}, "reverb_send": 0.3}}),
        json!({"op": "add_bus", "name": "group", "tracks": ["pad", "hits"], "mixer": {
            "comp": {"threshold_db": -4.0, "relative": true, "ratio": 2.5, "attack_ms": 5.0, "release_ms": 120.0, "lookahead_ms": 5.0},
            "dyn_eq": [{"kind": "peak", "hz": 3000.0, "q": 1.5, "threshold_db": -6.0, "range_db": 6.0}],
            "deess": {"hz": 6000.0, "threshold_db": -8.0, "range_db": 6.0},
            "reverb": reverb("hall", 0.15)}}),
        json!({"op": "add_bus", "name": "verb", "sends": [{"track": "pad", "level": 0.3}, {"track": "hits", "level": 0.5}],
            "mixer": {"reverb": reverb("plate", 1.0), "gain_db": -6.0}}),
        json!({"op": "set_master", "master": {
            "glue": {"threshold_db": -3.0, "relative": true, "ratio": 2.0, "attack_ms": 10.0, "release_ms": 150.0, "lookahead_ms": 2.0},
            "multiband": {"crossovers_hz": [200.0, 2500.0], "bands": [
                {"threshold_db": -3.0, "ratio": 2.0}, {"threshold_db": -4.0, "ratio": 1.5}, {"threshold_db": -6.0, "ratio": 2.5}]},
            "mono_below_hz": 150.0, "width": 1.2, "limiter": {"lookahead_ms": 5.0, "release_ms": 80.0, "auto_release": true, "clip_db": 1.0},
            "tail_s": 2.0}}),
    ];
    let mut p = Project::new("mixing-test", 120.0, 4);
    p.sample_rate = SR;
    for c in serde_json::from_value::<Vec<Command>>(Value::Array(cmds)).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn play_live(p: &Project, hints: RenderHints, secs: f64) -> Stereo {
    let mut e = LiveEngine::new(Arc::new(p.clone()), hints);
    let t0 = Instant::now();
    while e.loading() > 0 && t0.elapsed() < Duration::from_secs(30) {
        e.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(e.loading(), 0, "the audio didn't load");
    let n = s(secs);
    let (mut l, mut r, mut pos) = (vec![0f32; n], vec![0f32; n], vec![0u32; n]);
    assert_eq!(e.render(&mut l, &mut r, &mut pos), n);
    (l, r)
}

fn rms_db(x: &[f32]) -> f64 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64 + 1e-30).log10()
}

/// How far `a` is from `b`, as the level of their difference under `b`'s (dB).
fn residual_db(a: &[f32], b: &[f32]) -> f64 {
    let d: Vec<f32> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    rms_db(&d) - rms_db(b)
}

#[test]
fn live_playback_through_buses_and_the_master_chain_matches_the_render() {
    for looped in [false, true] {
        let p = project(looped);
        let out = render(&p, &RenderOptions::default()).unwrap();
        assert!((out.lufs - p.master.target_lufs).abs() < 0.2, "looped={looped}: rendered at {:.2} LUFS", out.lufs);
        let live = play_live(&p, RenderHints::from_render(&out), 7.5);
        // once the loudness drive has glided in, live playback is the render, sample for sample (near enough)
        for (a, b) in [(2.0, 3.0), (4.0, 5.5), (6.0, 7.5)] {
            let (w, ch) = (s(a)..s(b), [(&live.0, &out.left), (&live.1, &out.right)]);
            for (c, (x, y)) in ch.into_iter().enumerate() {
                let (lv, of) = (rms_db(&x[w.clone()]), rms_db(&y[w.clone()]));
                let res = residual_db(&x[w.clone()], &y[w.clone()]);
                println!("looped={looped} {a}-{b} s ch{c}: live {lv:.2} dB, rendered {of:.2} dB, residual {res:.1} dB");
                assert!((lv - of).abs() < 0.2, "looped={looped} {a}-{b} s ch{c}: live {lv:.2} dB, rendered {of:.2} dB");
                assert!(res < -30.0, "looped={looped} {a}-{b} s ch{c}: live is off the render by {res:.1} dB");
            }
        }
    }
}

/// The `project` with every stage of the master on: a 24 dB/octave high-pass, a side-only high shelf and a mid-only
/// cut in its EQ (minimum or linear phase), a dynamic EQ band on the mid, the de-esser, a multiband compressor with a
/// mid band, a parallel band and a soft-kneed one, the exciter (tape and tube bands) and the imager.
fn mastered(looped: bool, phase: &str) -> Project {
    let mut p = project(looped);
    let cmd = json!({"op": "set_master", "master": {
        "eq": [{"kind": "high_pass", "hz": 35.0, "db": 0.0, "slope": 24},
            {"kind": "high_shelf", "hz": 7000.0, "db": 2.5, "q": 0.7, "channel": "side"},
            {"kind": "peak", "hz": 400.0, "db": -2.0, "q": 1.2, "channel": "mid"}],
        "eq_phase": phase,
        "dyn_eq": [{"kind": "peak", "hz": 3000.0, "q": 1.4, "threshold_db": -4.0, "range_db": -4.0, "channel": "mid"}],
        "deess": {"hz": 6500.0, "threshold_db": -6.0, "range_db": -5.0},
        "multiband": {"crossovers_hz": [200.0, 2500.0], "bands": [
            {"threshold_db": -3.0, "ratio": 2.0, "channel": "mid"}, {"threshold_db": -4.0, "ratio": 3.0, "mix": 0.5},
            {"threshold_db": -6.0, "ratio": 2.5, "knee_db": 12.0}]},
        "exciter": {"crossovers_hz": [300.0, 4000.0], "bands": [
            {"drive_db": 6.0, "mix": 0.3, "mode": "tube"}, {"drive_db": 0.0, "mix": 0.0}, {"drive_db": 9.0, "mix": 0.4, "mode": "tape"}]},
        "imager": {"crossovers_hz": [250.0, 5000.0], "widths": [0.3, 1.0, 1.4]}}});
    commands::apply(&mut p, &serde_json::from_value::<Command>(cmd).unwrap()).unwrap();
    p
}

#[test]
fn the_whole_mastering_chain_plays_live_as_it_renders_and_its_stems_sum_to_it() {
    for (looped, phase) in [(true, "minimum"), (false, "linear"), (true, "linear")] {
        let p = mastered(looped, phase);
        let opts = RenderOptions { layers: true, ..RenderOptions::default() };
        let out = render(&p, &opts).unwrap();
        let tag = format!("looped={looped} {phase}");
        assert!((out.lufs - p.master.target_lufs).abs() < 0.2, "{tag}: rendered at {:.2} LUFS", out.lufs);
        assert!(out.true_peak_dbtp <= p.master.ceiling_dbtp + 0.01, "{tag}: true peak {:.2} dBTP", out.true_peak_dbtp);
        let lv = &out.master_loud;
        assert!(lv.dyn_eq.len() == 1 && lv.deess.is_some() && lv.multiband.len() == 3 && lv.exciter.len() == 3, "{tag}: {lv:?}");
        let sum = out.layer_sum_error_db.expect("layers");
        assert!(sum < -60.0, "{tag}: the layers are off the master by {sum:.1} dBFS");
        let live = play_live(&p, RenderHints::from_render(&out), 7.5);
        for (a, b) in [(2.0, 3.0), (4.0, 5.5), (6.0, 7.5)] {
            let (w, ch) = (s(a)..s(b), [(&live.0, &out.left), (&live.1, &out.right)]);
            for (c, (x, y)) in ch.into_iter().enumerate() {
                let (lv, of) = (rms_db(&x[w.clone()]), rms_db(&y[w.clone()]));
                let res = residual_db(&x[w.clone()], &y[w.clone()]);
                println!("{tag} {a}-{b} s ch{c}: live {lv:.2} dB, rendered {of:.2} dB, residual {res:.1} dB");
                assert!((lv - of).abs() < 0.2, "{tag} {a}-{b} s ch{c}: live {lv:.2} dB, rendered {of:.2} dB");
                assert!(res < -30.0, "{tag} {a}-{b} s ch{c}: live is off the render by {res:.1} dB");
            }
        }
    }
}

fn apply(name: &str, cmds: Vec<Value>) -> Project {
    let mut p = Project::new(name, 120.0, 4);
    p.sample_rate = SR;
    for c in serde_json::from_value::<Vec<Command>>(Value::Array(cmds)).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

#[test]
fn every_route_lines_up_in_time() {
    // three copies of the same bursts: one straight to the master, one through a bus, one only through an aux send,
    // each route with lookahead compressors (at 1:1, so they change nothing but the timing). If the routes are lined
    // up, the mix is the three copies played straight to the master.
    let b = wav(&bursts(8.0));
    let unity = |ms: f64| json!({"threshold_db": 0.0, "ratio": 1.0, "lookahead_ms": ms});
    let mut cmds = vec![json!({"op": "set_project", "bars": 4, "looped": false})];
    for name in ["direct", "grouped", "sent"] {
        cmds.push(json!({"op": "add_track", "name": name, "instrument": {"kind": "audio", "path": b}}));
    }
    cmds.push(json!({"op": "set_master", "master": {"glue": null, "tail_s": 1.0}}));
    let reference = render(&apply("routes-reference", cmds.clone()), &RenderOptions::default()).unwrap();
    cmds.extend([
        json!({"op": "set_mixer", "track": "grouped", "mixer": {"comp": unity(4.0)}}),
        json!({"op": "add_bus", "name": "group", "tracks": ["grouped"], "mixer": {"comp": unity(7.0)}}),
        // only the send reaches the master: its output goes to a muted bus
        json!({"op": "add_bus", "name": "nowhere", "tracks": ["sent"], "mixer": {"mute": true}}),
        json!({"op": "add_bus", "name": "aux", "sends": [{"track": "sent", "level": 1.0}], "mixer": {"comp": unity(2.0)}}),
    ]);
    let p = apply("routes", cmds);
    let out = render(&p, &RenderOptions::default()).unwrap();
    let live = play_live(&p, RenderHints::from_render(&out), 7.0);
    let w = s(1.0)..s(7.0);
    for (c, (x, y, z)) in [(&live.0, &out.left, &reference.left), (&live.1, &out.right, &reference.right)].into_iter().enumerate() {
        let (offline, on) = (residual_db(&y[w.clone()], &z[w.clone()]), residual_db(&x[w.clone()], &z[w.clone()]));
        println!("ch{c}: the render is off the straight mix by {offline:.1} dB, live by {on:.1} dB");
        assert!(offline < -60.0 && on < -60.0, "ch{c}: the render is off the straight mix by {offline:.1} dB, live by {on:.1} dB");
    }
}
