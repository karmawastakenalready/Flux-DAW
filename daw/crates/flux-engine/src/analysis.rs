//! Measurements on a render: agents can't listen, so they read these.

use crate::render::RenderOutput;
use anyhow::Result;
use flux_core::Project;
use rustfft::{num_complex::Complex, FftPlanner};
use serde_json::{json, Value};

fn lufs_slice(out: &RenderOutput, a: usize, b: usize) -> f64 {
    if b <= a + out.sr as usize / 2 {
        return f64::NAN;
    }
    crate::render::measure(&out.left[a..b], &out.right[a..b], out.sr).map(|x| x.0).unwrap_or(f64::NAN)
}

fn r1(x: f64) -> Value {
    if x.is_finite() {
        json!((x * 10.0).round() / 10.0)
    } else {
        Value::Null
    }
}

pub fn loudness(out: &RenderOutput) -> Value {
    let clipped = out.left.iter().chain(out.right.iter()).filter(|x| x.abs() >= 0.99997).count();
    let thr = 10f32.powf(-60.0 / 20.0);
    let n = out.left.len();
    let head = (0..n).find(|&k| out.left[k].abs() > thr || out.right[k].abs() > thr).unwrap_or(n);
    let tail = (0..n).rev().find(|&k| out.left[k].abs() > thr || out.right[k].abs() > thr).map(|k| n - 1 - k).unwrap_or(n);
    let sp = out.left.iter().chain(out.right.iter()).fold(0f32, |m, x| m.max(x.abs()));
    json!({
        "integrated_lufs": r1(out.lufs), "true_peak_dbtp": r1(out.true_peak_dbtp),
        "sample_peak_dbfs": r1(20.0 * (sp as f64).max(1e-12).log10()), "clipped_samples": clipped,
        "head_silence_ms": r1(head as f64 * 1000.0 / out.sr as f64), "tail_silence_ms": r1(tail as f64 * 1000.0 / out.sr as f64),
        "duration_s": r1(out.duration_s()), "looped": out.looped, "seam_error_db": out.seam_error_db.map(r1), "seam_crossfade_ms": out.seam_crossfade_ms.map(r1),
        "master_drive_db": r1(out.drive_db), "limiter_max_gr_db": r1(out.limiter_gr_db),
    })
}

pub fn sections(out: &RenderOutput) -> Value {
    Value::Array(
        out.sections
            .iter()
            .map(|s| json!({"name": s.name, "bars": format!("{}-{}", s.start_bar, s.start_bar + s.bars - 1),
                            "start_s": r1(s.start as f64 / out.sr as f64), "lufs": r1(lufs_slice(out, s.start, s.end))}))
            .collect(),
    )
}

/// Short-term loudness per bar - shows dynamics (dropouts, slams) at a glance.
pub fn bars(out: &RenderOutput) -> Value {
    let mut v = vec![];
    for (i, &a) in out.bar_starts.iter().enumerate() {
        let b = out.bar_starts.get(i + 1).copied().unwrap_or(out.left.len()).min(out.left.len());
        let mut s = 0f64;
        for k in a..b {
            let m = (out.left[k] as f64 + out.right[k] as f64) * 0.5;
            s += m * m;
        }
        let rms = 10.0 * (s / (b.saturating_sub(a)).max(1) as f64 + 1e-24).log10();
        v.push(json!({"bar": i + 1, "rms_db": r1(rms)}));
    }
    Value::Array(v)
}

pub fn balance(out: &RenderOutput) -> Value {
    let names: Vec<String> = out.sections.iter().map(|s| s.name.clone()).collect();
    let rows: Vec<Value> = out
        .tracks
        .iter()
        .map(|t| {
            let mut o = serde_json::Map::new();
            o.insert("track".into(), json!(t.name));
            o.insert("engine".into(), json!(t.engine));
            o.insert("peak_dbfs".into(), r1(t.peak_dbfs));
            for (i, n) in names.iter().enumerate() {
                o.insert(n.clone(), t.section_rms_db.get(i).map(|x| if *x < -100.0 { Value::Null } else { r1(*x) }).unwrap_or(Value::Null));
            }
            if t.layer != "base" {
                o.insert("layer".into(), json!(t.layer));
            }
            if let Some(tb) = &t.timbre {
                o.insert("hi3k_db".into(), r1(tb.hi3k_db));
                o.insert("centroid_hz".into(), json!(tb.centroid_hz.round()));
                if let Some(a) = tb.attack_db {
                    o.insert("attack_db".into(), r1(a));
                }
            }
            if let Some(w) = &t.warning {
                o.insert("warning".into(), json!(w));
            }
            Value::Object(o)
        })
        .collect();
    json!({"sections": names, "unit": "post-fader RMS dBFS per section (null = silent)",
        "timbre": "hi3k_db = energy above 3 kHz vs total (lower = darker); attack_db = note-start spike over the body (lower = softer)",
        "tracks": rows})
}

pub fn bands(out: &RenderOutput) -> Value {
    let seg = 8192usize;
    let n = out.left.len() / seg;
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(seg);
    let mut spec = vec![0f64; seg / 2 + 1];
    let win: Vec<f64> = (0..seg).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / seg as f64).cos()).collect();
    for i in 0..n {
        let mut buf: Vec<Complex<f64>> = (0..seg)
            .map(|j| Complex::new((out.left[i * seg + j] as f64 + out.right[i * seg + j] as f64) * 0.5 * win[j], 0.0))
            .collect();
        fft.process(&mut buf);
        for (k, s) in spec.iter_mut().enumerate() {
            *s += buf[k].norm_sqr();
        }
    }
    let tot: f64 = spec.iter().sum::<f64>().max(1e-30);
    let mut o = serde_json::Map::new();
    for c in [63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0] {
        let lo = c / 2f64.sqrt();
        let hi = c * 2f64.sqrt();
        let e: f64 = spec
            .iter()
            .enumerate()
            .filter(|(k, _)| {
                let f = *k as f64 * out.sr as f64 / seg as f64;
                f >= lo && f < hi
            })
            .map(|(_, s)| s)
            .sum();
        o.insert(format!("{} Hz", c as u32), r1(10.0 * (e / tot + 1e-30).log10()));
    }
    json!({"unit": "dB relative to total energy", "bands": o})
}

pub fn all(p: &Project, out: &RenderOutput) -> Result<Value> {
    Ok(json!({
        "project": p.name, "loudness": loudness(out), "sections": sections(out),
        "balance": balance(out), "bands": bands(out),
    }))
}
