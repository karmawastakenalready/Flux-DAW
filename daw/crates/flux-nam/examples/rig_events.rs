//! Prints the source events of a render job as JSON, in the form `rig.py` hands them to `SfizzSynth.render`
//! (after its sort): `{"preroll": N, "events": [[frame, "on"|"off"|"cc"|"pb", a, b], ...]}`.
//!
//! ```text
//! cargo run -p flux-nam --example rig_events -- <rig.json> <root> <job.json>
//! ```
//!
//! The job uses `rig.py`'s format: `{"sample_rate": 48000, "notes": [[start_s, end_s, pitch, velocity], ...],
//! "ccs": [[time_s, cc, value], ...], "bends": [[time_s, value], ...]}`.

use flux_nam::{EventKind, JobBend, JobCc, JobNote, Rig, source_events};
use serde_json::{Value, json};

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

fn rows<'a>(job: &'a Value, key: &str) -> Vec<&'a [Value]> {
    job.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|r| r.as_array().map(Vec::as_slice).unwrap_or_else(|| fail(format!("{key}: rows must be arrays")))).collect())
        .unwrap_or_default()
}

fn num(v: &Value, what: &str) -> f64 {
    v.as_f64().unwrap_or_else(|| fail(format!("{what}: expected a number, got {v}")))
}

fn midi(v: &Value, what: &str) -> u8 {
    let x = num(v, what);
    if !(0.0..=127.0).contains(&x) || x.fract() != 0.0 {
        fail(format!("{what}: expected 0..=127, got {v}"));
    }
    x as u8
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [rig_path, root, job_path] = args.as_slice() else { fail("usage: rig_events <rig.json> <root> <job.json>") };
    let rig = Rig::load(rig_path, root).unwrap_or_else(|e| fail(e));
    let job: Value = serde_json::from_str(&std::fs::read_to_string(job_path).unwrap_or_else(|e| fail(format!("{job_path}: {e}"))))
        .unwrap_or_else(|e| fail(format!("{job_path}: {e}")));
    let sr = job.get("sample_rate").and_then(Value::as_u64).unwrap_or_else(|| fail("job.sample_rate missing")) as u32;
    let notes: Vec<JobNote> = rows(&job, "notes")
        .iter()
        .map(|r| JobNote {
            start_s: num(&r[0], "note start"),
            end_s: num(&r[1], "note end"),
            pitch: midi(&r[2], "note pitch"),
            velocity: midi(&r[3], "note velocity"),
        })
        .collect();
    let ccs: Vec<JobCc> = rows(&job, "ccs")
        .iter()
        .map(|r| JobCc { time_s: num(&r[0], "cc time"), cc: midi(&r[1], "cc number"), value: midi(&r[2], "cc value") })
        .collect();
    let bends: Vec<JobBend> =
        rows(&job, "bends").iter().map(|r| JobBend { time_s: num(&r[0], "bend time"), value: num(&r[1], "bend") as i32 }).collect();
    let se = source_events(&rig.source, &notes, &ccs, &bends, sr);
    let events: Vec<Value> = se
        .events
        .iter()
        .map(|e| match e.kind {
            EventKind::NoteOn { key, velocity } => json!([e.frame, "on", key, velocity]),
            EventKind::NoteOff { key, velocity } => json!([e.frame, "off", key, velocity]),
            EventKind::Cc { cc, value } => json!([e.frame, "cc", cc, value]),
            EventKind::PitchBend { value } => json!([e.frame, "pb", value, 0]),
        })
        .collect();
    println!("{}", json!({"preroll": se.preroll, "events": events}));
}
