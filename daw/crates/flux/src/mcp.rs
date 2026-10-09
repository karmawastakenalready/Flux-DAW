//! Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0).

use crate::state::Shared;
use crate::tools;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::Arc;

const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const INSTRUCTIONS: &str = "\
Flux is an agent-first DAW: you can control everything (composition, arrangement, instruments, mixing, \
mastering, export). A human may be watching the GUI and editing at the same time; their edits appear in \
`activity` with source=human, so check it before overwriting their work.

Workflow: get_project -> edit (a batch of commands; atomic; one undo step) -> render -> analyze -> adjust -> export.

Composing: compose writes musical material in one call (a melody from notation, chords voiced from a chart, a bass \
line or arpeggio over the chord track, phrasing, swells and CC/bend curves) as one undo step; re-running an action \
replaces what it wrote. lint finds what a listener would hear as a mistake (clashes, non-chord tones on strong \
beats, notes outside an instrument's range, mud, parallels...) from the score alone, before rendering.

Time: bars are 1-based. Inside a bar, `beat` is a 0-based offset in quarter notes (a 4/4 bar spans beats 0-4). \
Without `bar`, `beat` counts from the start of the song. Durations are in beats. Pitches are MIDI numbers or \
names (C4 = 60, sharps '#', flats 'b').

Instruments: sfz (sampled instruments played by sfizz: find them with list_instruments and check the playable \
range with probe_instrument before writing parts), vst3 (a VST3 instrument plugin hosted by Flux: it gets the \
song's tempo and position, so synced LFOs, arpeggiators and delays follow the song), audio (a WAV placed at time \
0) or none. A VST3 instrument's sound comes from its presets: plugins presets/load_preset reads the plugin's own \
preset files (Vital banks, Decent Sampler libraries...) headless, and set_program loads factory programs. A rig (sfz/rigs/*.rig.json; list_instruments kind=rig, details=true describes it) is a sampled guitar or \
bass played into Neural Amp Modeler amp captures and cabinets: give its path as the vst3 instrument's path; track \
params set its stages by id or type (e.g. {\"nam.input_db\": 3} drives the amp harder) and are heard live at once. \
Imported pipeline songs use audio stems; their MIDI is kept, and `sfz_hint` names \
the SFZ that made each stem: switch the instrument to it (update_track) to make note edits audible. \
probe_instrument with render=false describes an SFZ (keys, keyswitches, layers, controllers) at once; sfz_sets \
checks and builds the song wrapper sets in build/sfz/, and sfz_derive writes tightened, retuned or level-evened \
copies to sfz/derived/ (instruments/ is never changed).

Mixing: set_mixer per track (gain, pan, width, HPF/LPF, EQ, drive, gate, compressor, delay, sidechain duck, reverb \
send). set_master holds glue compression, EQ, the shared reverb, the loudness target (default -15 LUFS) and the \
true-peak ceiling (default -1 dBTP). VST3 effects go in insert slots (edit add_insert): on a track (pre or post its \
strip), on the master, or on the reverb return (which then replaces the built-in reverb); plugins sets them up (params, \
factory programs, presets in the plugin's own format such as Valhalla's or Vital's, or its window for the human), \
latency is compensated, and a failing insert passes audio through with a warning. The master drives the mix to the target automatically, so balance tracks \
against each other; limiter_max_gr_db beyond about -4 dB means the mix is too peaky.

You can't listen, so measure: analyze gives per-section loudness, per-track RMS per section, octave bands and \
per-bar loudness (dropouts and slams show there), and gui screenshot shows the screen. mix_check names the \
tracks behind masking, buried parts, mud, harshness, reverb smear and mono loss; reference_match compares the mix \
with a reference track (keep them in refs/) at equal loudness; spectrogram draws the master, the dry mix, tracks or \
files; game_audio_check judges all the game's exported music together (loudness target, peaks, loop seams, layer \
sums, file size); instrument_card measures an SFZ (range, levels, tuning, attack) before you choose it. Looped \
projects render as seamless loops (seam_error_db should be below -60).";

pub fn serve(shared: Arc<Shared>, mut out: std::fs::File) {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let replies: Vec<Value> = match serde_json::from_str::<Value>(text) {
            Err(e) => vec![json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})],
            Ok(Value::Array(batch)) => batch.into_iter().filter_map(|m| handle(&shared, m)).collect(),
            Ok(msg) => handle(&shared, msg).into_iter().collect(),
        };
        for r in replies {
            let mut s = serde_json::to_string(&r).unwrap_or_default();
            s.push('\n');
            if out.write_all(s.as_bytes()).and_then(|_| out.flush()).is_err() {
                return;
            }
        }
    }
}

fn handle(shared: &Arc<Shared>, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    // replies to our own requests (we send none) have no method
    let method = msg.get("method")?.as_str()?.to_string();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result: Result<Value, (i64, String)> = match method.as_str() {
        "initialize" => Ok(initialize(shared, &params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::list()})),
        "tools/call" => Ok(tools::call(shared, &params)),
        "resources/list" => Ok(json!({"resources": []})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        m if m.starts_with("notifications/") => return None,
        m => Err((-32601, format!("method not found: {m}"))),
    };
    let id = id?;
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn initialize(shared: &Arc<Shared>, params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or("");
    let version = PROTOCOLS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOLS[1]);
    let client = params["clientInfo"]["name"].as_str().unwrap_or("agent").to_string();
    {
        let mut st = shared.lock();
        st.agent = Some(client.clone());
        st.session.note(flux_core::Source::System, format!("agent connected: {client}"));
        Shared::repaint(&st);
    }
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "flux", "title": "Flux DAW", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}
