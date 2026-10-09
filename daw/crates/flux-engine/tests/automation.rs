//! Volume, pan and reverb-send lanes in offline renders and live playback, and audio clips played live. Audio
//! tracks only, so no instruments are needed.

use flux_core::{Command, Project, commands};
use flux_engine::dsp::Stereo;
use flux_engine::live::{LiveEngine, RenderHints};
use flux_engine::render::write_wav_f32;
use flux_engine::{RenderOptions, render};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SR: u32 = 48_000;

fn s(t: f64) -> usize {
    (t * SR as f64) as usize
}

/// A steady 220 Hz tone, the same in both channels.
fn tone(len_s: f64) -> Stereo {
    let l: Vec<f32> = (0..s(len_s)).map(|k| (0.2 * (std::f64::consts::TAU * 220.0 * k as f64 / SR as f64).sin()) as f32).collect();
    (l.clone(), l)
}

/// Short decaying bursts at the given times (seconds).
fn bursts(times: &[f64], len_s: f64) -> Stereo {
    let mut l = vec![0f32; s(len_s)];
    for &t in times {
        for k in 0..s(0.05) {
            let x = k as f64 / SR as f64;
            l[s(t) + k] += (0.4 * (-x * 60.0).exp() * (std::f64::consts::TAU * 440.0 * x).sin()) as f32;
        }
    }
    (l.clone(), l)
}

fn project(looped: bool, audio: &Stereo, cmds: Vec<Value>) -> Project {
    let d = std::env::temp_dir().join(format!("flux-automation-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    // the tests run at once: a file per sound
    let f = d.join(format!("{:016x}.wav", audio.0.iter().fold(0u64, |h, x| h.rotate_left(5) ^ u64::from(x.to_bits()))));
    write_wav_f32(&f, SR, audio).unwrap();
    let mut all = vec![
        json!({"op": "set_project", "bars": 4, "looped": looped}),
        json!({"op": "add_track", "name": "a", "instrument": {"kind": "audio", "path": f.to_string_lossy()}}),
        json!({"op": "set_master", "master": {"glue": null, "tail_s": 2.0}}),
    ];
    all.extend(cmds);
    let mut p = Project::new("automation-test", 120.0, 4);
    p.sample_rate = SR;
    for c in serde_json::from_value::<Vec<Command>>(Value::Array(all)).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn rms_db(x: &[f32]) -> f64 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64 + 1e-30).log10()
}

#[test]
fn a_volume_lane_fades_the_track() {
    // 4 bars at 120 bpm: 8 s. Silent at the top, up to 0 dB at beat 8 (4 s), straight in dB
    let p = project(false, &tone(8.0), vec![json!({"op": "add_automation", "track": "a", "target": "volume",
        "points": [{"beat": 0, "value": -90}, {"beat": 8, "value": 0}]})]);
    let out = render(&p, &RenderOptions::default()).unwrap();
    let full = rms_db(&out.left[s(5.0)..s(6.0)]);
    let start = rms_db(&out.left[s(0.0)..s(0.2)]);
    let half = rms_db(&out.left[s(1.95)..s(2.05)]);
    assert!(full - start > 60.0, "the start should be near silent: {start:.1} vs {full:.1} dB");
    assert!((half - full - -45.0).abs() < 1.5, "half way should be 45 dB down: {:.1}", half - full);
}

#[test]
fn a_pan_lane_moves_the_track() {
    // hard left, then a jump to hard right at beat 8
    let p = project(false, &tone(8.0), vec![json!({"op": "add_automation", "track": "a", "target": "pan",
        "points": [{"beat": 0, "value": -1, "shape": "hold"}, {"beat": 8, "value": 1}]})]);
    let out = render(&p, &RenderOptions::default()).unwrap();
    let (l1, r1) = (rms_db(&out.left[s(1.0)..s(3.0)]), rms_db(&out.right[s(1.0)..s(3.0)]));
    let (l2, r2) = (rms_db(&out.left[s(5.0)..s(7.0)]), rms_db(&out.right[s(5.0)..s(7.0)]));
    assert!(l1 - r1 > 60.0 && r2 - l2 > 60.0, "left {l1:.1}/{r1:.1}, right {l2:.1}/{r2:.1}");
    // the jump is a short ramp, not a click: no sample-to-sample step bigger than the tone's own
    let at = s(4.0);
    let step = (at - 500..at + 500).map(|k| (out.right[k + 1] - out.right[k]).abs()).fold(0f32, f32::max);
    let tone_step = (s(5.0)..s(5.0) + 1000).map(|k| (out.right[k + 1] - out.right[k]).abs()).fold(0f32, f32::max);
    assert!(step <= tone_step * 1.05, "step {step} vs the tone's {tone_step}");
}

#[test]
fn a_send_lane_feeds_the_reverb() {
    // no send for the first hit, all of it for the second; the stems still sum to the master
    for looped in [false, true] {
        let p = project(looped, &bursts(&[1.0, 5.0], 8.0), vec![json!({"op": "add_automation", "track": "a", "target": "reverb_send",
            "points": [{"beat": 0, "value": 0, "shape": "hold"}, {"beat": 8, "value": 1}]})]);
        let out = render(&p, &RenderOptions { track_stems: true, layers: true, ..Default::default() }).unwrap();
        let dry_tail = rms_db(&out.left[s(1.2)..s(1.6)]);
        let wet_tail = rms_db(&out.left[s(5.2)..s(5.6)]);
        assert!(wet_tail - dry_tail > 30.0, "looped={looped}: reverb after the second hit {wet_tail:.1}, after the first {dry_tail:.1}");
        let stem = &out.track_stems[0].1;
        let err = stem.0.iter().zip(&out.left).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-4, "looped={looped}: the stem is off the master by {err}");
        assert!(out.layer_sum_error_db.unwrap() < -90.0);
    }
}

#[test]
fn lanes_wrap_around_a_loop() {
    // a fade in over the whole loop: the loop's end runs back into its silent start, smoothly
    let p = project(true, &tone(8.0), vec![json!({"op": "add_automation", "track": "a", "target": "volume",
        "points": [{"beat": 0, "value": -40}, {"beat": 16, "value": 0}]})]);
    let out = render(&p, &RenderOptions::default()).unwrap();
    assert_eq!(out.left.len(), s(8.0));
    assert!(out.seam_error_db.unwrap() < -60.0, "seam {:?}", out.seam_error_db);
    let end = rms_db(&out.left[s(7.8)..s(7.95)]);
    let start = rms_db(&out.left[s(0.05)..s(0.2)]);
    assert!((end - start - 39.0).abs() < 2.0, "end {end:.1}, start {start:.1}");
}

/// `p` played live from the top for `secs` (left, right), with the loudness drive its render measured.
fn play_live(p: &Project, secs: f64) -> (Vec<f32>, Vec<f32>, LiveEngine) {
    let out = render(p, &RenderOptions::default()).unwrap();
    let mut e = LiveEngine::new(Arc::new(p.clone()), RenderHints::from_render(&out));
    let t0 = Instant::now();
    while e.loading() > 0 && t0.elapsed() < Duration::from_secs(30) {
        e.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(e.loading(), 0, "the audio didn't load");
    let (l, r) = play(&mut e, secs);
    (l, r, e)
}

fn play(e: &mut LiveEngine, secs: f64) -> (Vec<f32>, Vec<f32>) {
    let n = s(secs);
    let (mut l, mut r, mut pos) = (vec![0f32; n], vec![0f32; n], vec![0u32; n]);
    assert_eq!(e.render(&mut l, &mut r, &mut pos), n);
    (l, r)
}

#[test]
fn live_playback_follows_the_lanes_as_the_render_does() {
    // a fade in over the first two bars, hard left until bar 3 then hard right
    let p = project(false, &tone(8.0), vec![
        json!({"op": "add_automation", "track": "a", "target": "volume", "points": [{"beat": 0, "value": -40}, {"beat": 8, "value": 0}]}),
        json!({"op": "add_automation", "track": "a", "target": "pan", "points": [{"beat": 0, "value": -1, "shape": "hold"}, {"beat": 8, "value": 1}]}),
    ]);
    let out = render(&p, &RenderOptions::default()).unwrap();
    let (l, r, mut e) = play_live(&p, 7.0);
    for (a, b) in [(0.5, 1.0), (1.9, 2.1), (3.0, 3.5)] {
        let (live, offline) = (rms_db(&l[s(a)..s(b)]), rms_db(&out.left[s(a)..s(b)]));
        assert!((live - offline).abs() < 0.5, "{a}-{b} s: live {live:.1} dB, rendered {offline:.1} dB");
    }
    let (rl, rr) = (rms_db(&l[s(5.0)..s(6.0)]), rms_db(&r[s(5.0)..s(6.0)]));
    let offline = rms_db(&out.right[s(5.0)..s(6.0)]);
    assert!(rr - rl > 60.0 && (rr - offline).abs() < 0.5, "after the jump: left {rl:.1}, right {rr:.1}, rendered {offline:.1} dB");
    assert!(rms_db(&r[s(1.0)..s(3.0)]) < rms_db(&l[s(1.0)..s(3.0)]) - 60.0, "hard left before it");
    // the jump glides over a block rather than clicking
    let step = (s(3.9)..s(4.1)).map(|k| (r[k + 1] - r[k]).abs()).fold(0f32, f32::max);
    let tone_step = (s(5.0)..s(5.0) + 1000).map(|k| (r[k + 1] - r[k]).abs()).fold(0f32, f32::max);
    assert!(step <= tone_step * 1.05, "step {step} vs the tone's {tone_step}");

    // an edit while playing is heard at once: without its volume lane the track plays from the top as loud as hard
    // right after the jump
    let mut q = p.clone();
    commands::apply(&mut q, &serde_json::from_value(json!({"op": "clear_automation", "track": "a", "target": "volume"})).unwrap()).unwrap();
    e.update(Arc::new(q));
    e.seek(0);
    let (l, _) = play(&mut e, 1.0);
    let now = rms_db(&l[s(0.5)..s(1.0)]);
    assert!((now - offline).abs() < 0.5, "{now:.1} vs {offline:.1} dB");
}

#[test]
fn live_playback_follows_a_send_lane() {
    // no send for the first hit, all of it for the second
    let p = project(true, &bursts(&[1.0, 5.0], 8.0), vec![json!({"op": "add_automation", "track": "a", "target": "reverb_send",
        "points": [{"beat": 0, "value": 0, "shape": "hold"}, {"beat": 8, "value": 1}]})]);
    let (l, _, _) = play_live(&p, 6.0);
    let dry_tail = rms_db(&l[s(1.2)..s(1.6)]);
    let wet_tail = rms_db(&l[s(5.2)..s(5.6)]);
    assert!(wet_tail - dry_tail > 30.0, "reverb after the second hit {wet_tail:.1}, after the first {dry_tail:.1}");
}

/// Clips play live where they are put, with their gain, and an edit to one is heard once the track is read again
/// (the old version plays on meanwhile).
#[test]
fn live_playback_plays_clips_and_follows_their_edits() {
    // the file's first half second, in bar 2 (2 s to 2.5 s)
    let edit = |p: &mut Project, v: Value| commands::apply(p, &serde_json::from_value(v).unwrap()).unwrap();
    let mut p = project(false, &tone(1.0), vec![json!({"op": "set_mixer", "track": "a", "mixer": {"reverb_send": 0}})]);
    let id = p.tracks[0].clips[0].id;
    edit(&mut p, json!({"op": "update_clip", "clip": id, "bar": 2, "length": 0.5}));
    let out = render(&p, &RenderOptions::default()).unwrap();
    let (l, _, mut e) = play_live(&p, 3.0);
    let (live, offline) = (rms_db(&l[s(2.1)..s(2.4)]), rms_db(&out.left[s(2.1)..s(2.4)]));
    assert!(live > -30.0 && (live - offline).abs() < 0.5, "live {live:.1}, rendered {offline:.1} dB");
    for (a, b) in [(0.1, 1.9), (2.6, 2.9)] {
        let quiet = rms_db(&l[s(a)..s(b)]);
        assert!(quiet < -80.0, "{a}-{b} s: {quiet:.1} dB");
    }

    // moved to the top, 6 dB down
    let mut q = p.clone();
    edit(&mut q, json!({"op": "update_clip", "clip": id, "bar": 1, "gain_db": -6.0}));
    e.update(Arc::new(q));
    let deadline = Instant::now() + Duration::from_secs(30);
    let now = loop {
        e.poll();
        e.seek(0);
        let (l, _) = play(&mut e, 0.5);
        let now = rms_db(&l[s(0.1)..s(0.4)]);
        if now > -60.0 || Instant::now() > deadline {
            break now;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!((now - (live - 6.0)).abs() < 0.5, "{now:.1} dB after the edit, {live:.1} before");
}
