//! Signals and files shared by the integration tests. Everything is synthesised: no recordings are needed.
#![allow(dead_code)]

use flux_analysis::AudioRef;
use flux_analysis::loudness::loudness_stats;
use std::f64::consts::PI;
use std::path::Path;

pub const SR: u32 = 48000;

/// Plucked notes on every beat at 120 BPM: a sine of `freq` Hz under an envelope that restarts every half second
/// and decays with a 100 ms time constant. Seamless as a loop when `secs` is a multiple of 0.5 and `freq / 2` is
/// a whole number (each beat starts on a zero crossing).
pub fn plucks(secs: f64, freq: f64) -> Vec<f32> {
    let n = (secs * SR as f64).round() as usize;
    let beat = SR as usize / 2;
    (0..n)
        .map(|i| {
            let since_beat = (i % beat) as f64 / SR as f64;
            ((-since_beat / 0.1).exp() * (2.0 * PI * freq * i as f64 / SR as f64).sin()) as f32
        })
        .collect()
}

/// A steady sine.
pub fn sine(secs: f64, freq: f64, amp: f64) -> Vec<f32> {
    let n = (secs * SR as f64).round() as usize;
    (0..n).map(|i| (amp * (2.0 * PI * freq * i as f64 / SR as f64).sin()) as f32).collect()
}

/// Integrated loudness of a dual-mono signal, LUFS.
pub fn lufs(x: &[f32]) -> f64 {
    loudness_stats(AudioRef::new(SR, x, x).unwrap(), false).unwrap().integrated_lufs.unwrap()
}

/// The gain that brings `x` to `target` LUFS.
pub fn gain_to(x: &[f32], target: f64) -> f32 {
    10f64.powf((target - lufs(x)) / 20.0) as f32
}

pub fn scaled(x: &[f32], g: f32) -> Vec<f32> {
    x.iter().map(|v| v * g).collect()
}

/// Write a dual-mono WAV: 32-bit float, or 16-bit PCM.
pub fn write_wav(path: &Path, x: &[f32], float: bool) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR,
        bits_per_sample: if float { 32 } else { 16 },
        sample_format: if float { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for &v in x {
        for _ in 0..2 {
            if float {
                w.write_sample(v).unwrap();
            } else {
                w.write_sample((v * 32767.0).round().clamp(-32768.0, 32767.0) as i16).unwrap();
            }
        }
    }
    w.finalize().unwrap();
}

/// A small game on disk, with known defects (see [`expected_flags`]):
///
/// - `explore_loop.wav`: a seamless loop at -15 LUFS (looped by its name; no bar times, so its seam is compared
///   with its own attacks);
/// - `battle_loop.wav`: the same, cut mid-note: the loop wraps with a jump;
/// - `field.wav`: a seamless loop that Flux's export report beside it (`field.report.json`) marks as looped;
/// - `town.wav`: a one-shot at -20 LUFS, 16-bit;
/// - `boss.layers.json`: a layered export (master `boss.wav`, layers `boss_drums.wav` and `boss_pads.wav`)
///   that sums exactly;
/// - `arena.layers.json`: a layered export with a stale layer loudness and a missing layer file;
/// - `broken.wav`: not audio; `notes.txt`: not a candidate.
pub fn write_game(dir: &Path) {
    let p = plucks(4.0, 220.0);
    let g = gain_to(&p, -15.0);
    let explore = scaled(&p, g);
    write_wav(&dir.join("explore_loop.wav"), &explore, true);
    // cut at 3.7511 s: 2.5 time constants into the last note, a quarter cycle past a zero crossing
    write_wav(&dir.join("battle_loop.wav"), &explore[..180_054], true);
    write_wav(&dir.join("field.wav"), &explore, true);
    std::fs::write(dir.join("field.report.json"), r#"{"loudness":{"lufs":-15.0,"looped":true}}"#).unwrap();
    let town = scaled(&p, gain_to(&p, -20.0));
    write_wav(&dir.join("town.wav"), &town, false);

    // layered exports: drums (plucks) and pads (a quiet steady fifth above)
    let pads = sine(4.0, 330.0, 0.1);
    let sum: Vec<f32> = p.iter().zip(&pads).map(|(a, b)| a + b).collect();
    let g = gain_to(&sum, -15.0);
    let (drums, pads) = (scaled(&p, g), scaled(&pads, g));
    let master: Vec<f32> = drums.iter().zip(&pads).map(|(a, b)| a + b).collect();
    for name in ["boss", "arena"] {
        write_wav(&dir.join(format!("{name}.wav")), &master, true);
        write_wav(&dir.join(format!("{name}_drums.wav")), &drums, true);
        if name == "boss" {
            write_wav(&dir.join(format!("{name}_pads.wav")), &pads, true);
        }
    }
    let layer = |name: &str, layer: &str, lufs: f64| {
        format!(r#"{{"layer":"{layer}","file":"{name}_{layer}.wav","lufs":{lufs},"peak_dbfs":-6.0,"bytes":1,"tracks":["{layer}"]}}"#)
    };
    let manifest = |name: &str, layers: Vec<String>| {
        format!(
            r#"{{"project":"{name}","master":"{name}.wav","sample_rate":48000,"duration_s":4.0,"looped":true,"loop_s":4.0,
            "tempo":[{{"bar":1,"bpm":120.0}}],"time_signature":[4,4],"bars":2,"bar_s":2.0,
            "sections":[{{"name":"A","start_bar":1,"bars":1,"start_s":0.0,"end_s":2.0}},{{"name":"B","start_bar":2,"bars":1,"start_s":2.0,"end_s":4.0}}],
            "layers":[{}],"sum_error_db":-150.0,"usage":"sum the layers"}}"#,
            layers.join(",")
        )
    };
    std::fs::write(dir.join("boss.layers.json"), manifest("boss", vec![layer("boss", "drums", lufs(&drums)), layer("boss", "pads", lufs(&pads))])).unwrap();
    std::fs::write(dir.join("arena.layers.json"), manifest("arena", vec![layer("arena", "drums", -40.0), layer("arena", "choir", -20.0)])).unwrap();
    std::fs::write(dir.join("broken.wav"), b"RIFF but not really").unwrap();
    std::fs::write(dir.join("notes.txt"), "not audio").unwrap();
}

/// Flag codes expected per track of [`write_game`], with loop detection on.
pub fn expected_flags() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("arena.wav", vec!["layer_stale", "layer_unusable", "layer_sum"]),
        ("battle_loop.wav", vec!["seam_jump"]),
        ("boss.wav", vec![]),
        ("explore_loop.wav", vec![]),
        ("field.wav", vec![]),
        ("town.wav", vec!["loudness_target", "loudness_outlier"]),
    ]
}
