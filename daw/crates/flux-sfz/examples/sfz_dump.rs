//! Dump the resolved regions of SFZ files as JSON lines, in the same shape as
//! `build/flux-sfz_check/scripts/sfizz_oracle.py` writes for sfizz, for the differential test.
//!
//! ```text
//! cargo run -p flux-sfz --example sfz_dump -- FILE_LIST OUT.jsonl
//! ```
//!
//! FILE_LIST holds one SFZ path per line.

use flux_sfz::opcode::{Support, support};
use flux_sfz::props::{KeyCenter, LoopMode, Trigger};
use flux_sfz::{Instrument, SampleRef};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;

fn trigger(t: Trigger) -> &'static str {
    match t {
        Trigger::Attack => "attack",
        Trigger::Release => "release",
        Trigger::ReleaseKey => "release_key",
        Trigger::First => "first",
        Trigger::Legato => "legato",
    }
}

fn loop_mode(m: LoopMode) -> &'static str {
    match m {
        LoopMode::NoLoop => "no_loop",
        LoopMode::OneShot => "one_shot",
        LoopMode::LoopContinuous => "loop_continuous",
        LoopMode::LoopSustain => "loop_sustain",
    }
}

fn dump(path: &str) -> Value {
    let inst = match Instrument::load(path) {
        Ok(i) => i,
        Err(e) => return json!({ "file": path, "loaded": false, "error": e.to_string() }),
    };
    let mut regions = vec![];
    for r in inst.playable_regions() {
        let p = r.props();
        let sample = match r.sample() {
            SampleRef::None => Value::Null,
            SampleRef::Generator { name } => json!(name),
            SampleRef::File(f) => json!(f.value),
        };
        let mut m = serde_json::Map::new();
        m.insert("sample".into(), sample);
        m.insert("key_range".into(), json!([p.lokey, p.hikey]));
        m.insert("vel_range".into(), json!([p.lovel, p.hivel]));
        m.insert(
            "pitch_keycenter".into(),
            match p.pitch_keycenter {
                KeyCenter::Key(k) => json!(k),
                KeyCenter::Sample => json!("sample"),
            },
        );
        if let Some((lo, hi)) = p.sw_last {
            m.insert("sw_last".into(), json!([lo, hi]));
        }
        if let Some(l) = &p.sw_label {
            m.insert("sw_label".into(), json!(l));
        }
        m.insert("volume".into(), json!(p.volume));
        m.insert("pan".into(), json!(p.pan));
        m.insert("width".into(), json!(p.width));
        m.insert("trigger".into(), json!(trigger(p.trigger)));
        m.insert("pitch".into(), json!(p.tune));
        m.insert("transpose".into(), json!(p.transpose));
        m.insert("seq_length".into(), json!(p.seq_length));
        m.insert("seq_position".into(), json!(p.seq_position));
        m.insert("rand_range".into(), json!([p.lorand, p.hirand]));
        m.insert("group".into(), json!(p.group));
        if let Some(o) = p.off_by {
            m.insert("off_by".into(), json!(o));
        }
        m.insert("amplitude".into(), json!(p.amplitude));
        if let Some(l) = p.loop_mode {
            m.insert("loop_mode".into(), json!(loop_mode(l)));
        }
        regions.push(Value::Object(m));
    }
    let mut unknown = BTreeSet::new();
    for b in inst.blocks() {
        if b.kind.is_region_level() {
            for o in b.opcodes.iter() {
                if support(b.kind, &o.name) == Support::Unknown {
                    unknown.insert(o.name.clone());
                }
            }
        }
    }
    json!({
        "file": path,
        "loaded": true,
        "regions": regions,
        "unknown": unknown,
        "cc_labels": inst.control().label_cc,
        "key_labels": inst.control().label_key,
        "errors": inst.errors().count(),
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, list, out] = args.as_slice() else {
        return Err("usage: sfz_dump FILE_LIST OUT.jsonl".into());
    };
    let files = std::fs::read_to_string(list)?;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    for f in files.lines().map(str::trim).filter(|l| !l.is_empty()) {
        writeln!(w, "{}", dump(f))?;
    }
    w.flush()?;
    Ok(())
}
