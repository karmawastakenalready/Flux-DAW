//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

/// The repository root (rigs and captures live there; tests that need them skip when they are absent).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("..")
}

/// A fresh directory under the system temp dir.
pub fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flux-nam-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// Deterministic noise in -0.5..0.5.
pub fn noise(n: usize, seed: u32) -> Vec<f32> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect()
}

/// A guitar-ish test signal: decaying tones with pauses.
pub fn guitar(n: usize, rate: f64) -> Vec<f32> {
    let hiss = noise(n, 3);
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let env = if t % 0.4 < 0.3 { (-(t % 0.4) * 8.0).exp() } else { 0.0 };
            (0.4 * env * (2.0 * std::f64::consts::PI * 82.4 * t).sin()) as f32 + 0.01 * hiss[i]
        })
        .collect()
}

/// Write a mono 32-bit float WAV.
pub fn write_wav(path: &Path, rate: u32, data: &[f32]) {
    let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(path, spec).expect("wav");
    for &v in data {
        w.write_sample(v).expect("wav sample");
    }
    w.finalize().expect("wav finalize");
}

/// A rig with every built-in stage kind: NAM (a fixture capture at 48 kHz), an IR, filters, dynamics, gain.
pub fn full_rig_json(ir: &Path) -> serde_json::Value {
    let model = fixtures().join("wavenet_real_like.nam");
    serde_json::json!({
        "source": {"sfz": "unused.sfz"},
        "chain": [
            {"type": "highpass", "hz": 45},
            {"type": "gate", "threshold_db": -50},
            {"id": "amp", "type": "nam", "model": model.to_string_lossy(), "input_db": 3, "output_db": -30},
            {"type": "ir", "path": ir.to_string_lossy(), "mix": 0.8},
            {"type": "peak", "hz": 2500, "db": -2},
            {"type": "lowshelf", "hz": 120, "db": 2},
            {"type": "highshelf", "hz": 7000, "db": -3},
            {"type": "comp"},
            {"type": "lowpass", "hz": 9000},
            {"type": "gain", "db": -3}
        ]
    })
}

/// A decaying-noise impulse response.
pub fn write_ir(dir: &Path, rate: u32) -> PathBuf {
    let n = (0.2 * rate as f64) as usize;
    let ir: Vec<f32> = noise(n, 17).iter().enumerate().map(|(i, v)| v * (-(i as f32) / (0.03 * rate as f32)).exp()).collect();
    let p = dir.join("ir.wav");
    write_wav(&p, rate, &ir);
    p
}
