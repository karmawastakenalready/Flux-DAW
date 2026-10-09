//! Run the crate's pitch analysis on a list of samples: the f0 estimate (comparable with
//! `tools/sample_pitch.py`) and the retune measurement against an expected key.
//!
//! ```text
//! cargo run -p flux-sfz --example pitch_check -- LIST.tsv OUT.jsonl
//! ```
//!
//! LIST.tsv lines: sample path, a tab, the expected MIDI key.

use flux_sfz::pitch::{PitchAnalyzer, analyze_sample, measure_sample_tuning};
use serde_json::json;
use std::io::Write;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, list, out] = args.as_slice() else {
        return Err("usage: pitch_check LIST.tsv OUT.jsonl".into());
    };
    let mut analyzer = PitchAnalyzer::new();
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    for line in std::fs::read_to_string(list)?.lines() {
        let Some((path, key)) = line.split_once('\t') else { continue };
        let key: u8 = key.trim().parse()?;
        let p = Path::new(path);
        let rec = match (analyze_sample(p, &mut analyzer), measure_sample_tuning(p, key, &mut analyzer)) {
            (Ok(e), Ok(t)) => json!({
                "path": path, "key": key, "duration": e.duration_s, "peak_db": e.peak_dbfs,
                "f0": e.f0_hz, "midi": e.midi, "tuning": t, "trusted": t.is_some_and(|t| t.trusted()),
            }),
            (Err(e), _) | (_, Err(e)) => json!({ "path": path, "key": key, "error": e.to_string() }),
        };
        writeln!(w, "{rec}")?;
    }
    w.flush()?;
    Ok(())
}
