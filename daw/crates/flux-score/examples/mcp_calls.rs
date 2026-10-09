//! Walks through the `compose` and `lint` MCP tools on a small project, printing each call's arguments and
//! result the way a server would return them.
//!
//! ```text
//! cargo run -p flux-score --example mcp_calls
//! ```

use flux_core::Project;
use flux_core::commands::apply;
use serde_json::{Value, json};

fn call_compose(project: &mut Project, args: Value) -> Result<(), String> {
    println!("compose {args}");
    let out = flux_score::compose(project, &args).map_err(|e| e.to_string())?;
    println!("  -> {}", out.to_json());
    println!("  label: {}", out.label);
    for c in &out.commands {
        apply(project, c).map_err(|e| format!("{}: {e}", c.describe()))?;
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut project: Project = serde_json::from_value(json!({
        "name": "demo", "tempo": [{"bar": 1, "bpm": 96.0}], "time_signature": [4, 4], "bars": 8, "key": "A minor",
        "tracks": [
            {"id": 1, "name": "Flute", "instrument": {"kind": "none"}},
            {"id": 2, "name": "Pad", "instrument": {"kind": "none"}},
            {"id": 3, "name": "Bass", "instrument": {"kind": "none"}}
        ]
    }))
    .map_err(|e| e.to_string())?;

    call_compose(&mut project, json!({"action": "harmony", "from_bar": 1, "chords": "Am | F | C | G E7"}))?;
    call_compose(&mut project, json!({"action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 4, "pattern": "x--x --x-", "step": 0.5}))?;
    call_compose(&mut project, json!({"action": "bass", "track": "Bass", "from_bar": 1, "to_bar": 4, "pattern": "x.o. 5.x."}))?;
    call_compose(&mut project, json!({"action": "melody", "track": "Flute", "bar": 1, "notation": "E5:2 C5:2 | A4:3 B4:1 | C5:2 G5:2 | F5:2 G#4:2"}))?;
    call_compose(&mut project, json!({"action": "fit", "track": "Flute", "from_bar": 1, "to_bar": 4}))?;
    call_compose(&mut project, json!({"action": "swell", "track": "Flute", "from_bar": 1, "to_bar": 4}))?;
    call_compose(&mut project, json!({"action": "harmony", "from_bar": 4, "to_bar": 4}))?;

    for args in [json!({"max_findings": 5}), json!({"format": "text", "checks": ["clash", "nct", "parallels"]})] {
        println!("lint {args}");
        let out = flux_score::lint_handler(&project, &args).map_err(|e| e.to_string())?;
        match out.get("text").and_then(Value::as_str) {
            Some(text) => print!("{text}"),
            None => println!("  -> {}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?),
        }
    }
    for args in [json!({"action": "chords", "track": "Organ", "from_bar": 1}), json!({"action": "melody", "track": "Flute", "bar": 1, "notation": "E5:2 H4:2"})] {
        println!("compose {args}");
        match flux_score::compose(&project, &args) {
            Ok(_) => println!("  -> ok"),
            Err(e) => println!("  -> error: {e}"),
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::from(1)
        }
    }
}
