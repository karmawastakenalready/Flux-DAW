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
`activity` with source=human, so check it before overwriting their work. They may also play parts in from a \
keyboard: while `transport` shows `recording`, their take lands on that track at each pass of the loop.

Workflow: get_project -> edit (a batch of commands; atomic; one undo step) -> render -> analyze -> adjust -> mix \
(mix_check) -> master -> export.

Composing: compose writes musical material in one call (a melody from notation, chords voiced from a chart, a bass \
line or arpeggio over the chord track, phrasing, swells and CC/bend curves) as one undo step; re-running an action \
replaces what it wrote. lint finds what a listener would hear as a mistake (clashes, non-chord tones on strong \
beats, notes outside an instrument's range, mud, parallels...) from the score alone, before rendering.

Time: bars are 1-based. Inside a bar, `beat` is a 0-based offset in quarter notes (a 4/4 bar spans beats 0-4). \
Without `bar`, `beat` counts from the start of the song. Durations are in beats. Pitches are MIDI numbers or \
names (C4 = 60, sharps '#', flats 'b').

Instruments: sfz (sampled instruments played by sfizz: find them with list_instruments and check the playable \
range with probe_instrument before writing parts), vst3 (a VST3 instrument plugin hosted by Flux: it gets the \
song's tempo and position, so synced LFOs, arpeggiators and delays follow the song), audio (audio files placed as \
clips: edit add_clip puts a WAV, FLAC or Ogg file anywhere on the track, trimmed, with gain and fades) or none. A VST3 instrument's sound comes from its presets: plugins presets/load_preset reads the plugin's own \
preset files (Vital banks, Decent Sampler libraries...) headless, and set_program loads factory programs. A rig (sfz/rigs/*.rig.json; list_instruments kind=rig, details=true describes it) is a sampled guitar or \
bass played into Neural Amp Modeler amp captures and cabinets: give its path as the vst3 instrument's path; track \
params set its stages by id or type (e.g. {\"nam.input_db\": 3} drives the amp harder) and are heard live at once. \
Imported pipeline songs use audio stems; their MIDI is kept, and `sfz_hint` names \
the SFZ that made each stem: switch the instrument to it (update_track) to make note edits audible. \
probe_instrument with render=false describes an SFZ (keys, keyswitches, layers, controllers) at once; sfz_sets \
checks and builds the song wrapper sets in build/sfz/, and sfz_derive writes tightened, retuned or level-evened \
copies to sfz/derived/ (instruments/ is never changed).

Mixing: set_mixer per track (gain, pan, width, HPF/LPF, EQ, drive, gate, compressor, dynamic EQ for a resonance \
that only bites when loud, de-esser at 5-8 kHz, delay, reverb, sidechain duck, reverb send) and its routing: \
`output` into a group bus, `sends` to aux returns. edit add_bus makes a bus: route tracks to it for shared EQ and \
glue, or give it a reverb at mix 1 and send tracks to it for an aux reverb (one space shared by many parts). \
set_master holds the shared reverb, glue compression, EQ, a multiband compressor, mono below, width, the limiter, \
the loudness target (default -15 LUFS) and the true-peak ceiling (default -1 dBTP). VST3 effects go in insert slots (edit add_insert): on a track (pre or post its \
strip), on the master, or on the reverb return (which then replaces the built-in reverb); plugins sets them up (params, \
factory programs, presets in the plugin's own format such as Valhalla's or Vital's, or its window for the human), \
latency is compensated, and a failing insert passes audio through with a warning. Automation (edit add_automation) \
moves a track's volume, pan or reverb send, or any VST3 parameter of its instrument or inserts (or of master and reverb-return \
inserts), over time: fades, swells, a filter opening, a reverb drying up into a drop; renders and live playback both follow it. \
The master drives the mix to the target automatically, so balance tracks \
against each other.

Mixing and mastering: balance levels and pans first, then buses and aux returns. mix_check finds what to fix per \
track, and mix_check apply=true (or apply=[indices]) carries out its fixes as one undo step and checks again; repeat \
until what is left is taste. Then master checks the master as a mastering engineer would and scores it 0-100 (100: \
nothing to fix; every lost point is an issue saying why, most with an indexed move): action=preset sets a chain \
(game_loop is the usual start), apply carries out the moves, auto repeats check and apply until the score stops \
rising, match EQs toward a reference. Export, then game_audio_check judges the files beside the game's other music. \
A relative threshold (glue, relative compressors, dynamic EQ, de-esser, multiband) counts from the signal's loud \
level (95th percentile of its short-term level): -4 means 4 dB under its loud moments, whatever the level; render's \
summary shows each loud level and threshold. The master runs: reverb return joins -> inserts -> glue -> EQ -> \
multiband -> mono below and width -> loudness drive and limiter (to the target and ceiling). A master EQ move stops \
at 6 dB: a region further off belongs in the mix (mix_check's regions name the tracks there). limiter_max_gr_db \
beyond about -4 dB means the mix is too peaky.

You can't listen, so measure: analyze gives per-section loudness, per-track RMS per section, octave bands and \
per-bar loudness (dropouts and slams show there), and gui screenshot shows the screen. mix_check names the \
tracks behind masking, buried parts, mud, harshness, reverb smear and mono loss; reference_match compares the mix \
with a reference track (keep them in refs/) at equal loudness; spectrogram draws the master, the dry mix, tracks or \
files; game_audio_check judges all the game's exported music together (loudness target, peaks, loop seams, layer \
sums, file size); instrument_card measures an SFZ (range, levels, tuning, attack) before you choose it. Looped \
projects render as seamless loops (seam_error_db should be below -60).

Not every instrument Flux supports is installed on every computer: list_instruments and plugins scan also list \
what is missing here (`not_installed`) and how the user gets it. Downloading is the user's call: suggest it when a \
song would gain from it, with its size. The flux://library resource lists every pack.";

/// The guide resource and prompt: the instructions, and where to find more.
fn guide() -> String {
    format!(
        "# Using Flux\n\n{INSTRUCTIONS}\n\n## More\n\nEvery tool's description in tools/list says what it takes and returns. The \
flux://library resource lists the instrument library's packs, what is installed, and how the user gets the rest. Projects are \
.flux.json files; renders and exports go to the exports folder (export says where)."
    )
}

const GUIDE_URI: &str = "flux://guide";
const LIBRARY_URI: &str = "flux://library";

fn resources() -> Value {
    json!([
        {"uri": GUIDE_URI, "name": "guide", "title": "Using Flux", "description": "How to drive Flux: workflow, time, instruments, mixing, mastering, measuring", "mimeType": "text/markdown"},
        {"uri": LIBRARY_URI, "name": "library", "title": "Instrument library", "description": "The instrument library's packs and plugins: what is installed here, and how the user gets the rest", "mimeType": "application/json"},
    ])
}

fn read_resource(params: &Value) -> Result<Value, (i64, String)> {
    let uri = params["uri"].as_str().unwrap_or("");
    let (mime, text) = match uri {
        GUIDE_URI => ("text/markdown", guide()),
        LIBRARY_URI => ("application/json", serde_json::to_string_pretty(&crate::supported::library_json(&crate::supported::packs())).unwrap_or_default()),
        // -32002: resource not found
        _ => return Err((-32002, format!("no resource {uri:?} (resources/list shows them)"))),
    };
    Ok(json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}))
}

fn prompts() -> Value {
    json!([{"name": "guide", "title": "Flux guide", "description": "Put Flux's guide into the conversation (for clients that don't show the server's instructions)"}])
}

fn get_prompt(params: &Value) -> Result<Value, (i64, String)> {
    match params["name"].as_str().unwrap_or("") {
        "guide" => Ok(json!({"description": "Flux guide", "messages": [{"role": "user", "content": {"type": "text", "text": guide()}}]})),
        n => Err((-32602, format!("no prompt {n:?} (prompts/list shows them)"))),
    }
}

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
        "resources/list" => Ok(json!({"resources": resources()})),
        "resources/read" => read_resource(&params),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "prompts/list" => Ok(json!({"prompts": prompts()})),
        "prompts/get" => get_prompt(&params),
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
        "capabilities": {"tools": {"listChanged": false}, "resources": {}, "prompts": {}},
        "serverInfo": {"name": "flux", "title": "Flux DAW", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{Project, Session};

    fn ask(method: &str, params: Value) -> Value {
        let sh = Shared::new(Session::new(Project::new("Test", 120.0, 8)));
        handle(&sh, json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).unwrap()
    }

    #[test]
    fn the_guide_is_a_resource_and_a_prompt() {
        let caps = &ask("initialize", json!({"protocolVersion": "2025-06-18"}))["result"]["capabilities"];
        assert!(caps["resources"].is_object() && caps["prompts"].is_object());
        let list = ask("resources/list", json!({}));
        let uris: Vec<&str> = list["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
        assert_eq!(uris, ["flux://guide", "flux://library"]);
        let read = ask("resources/read", json!({"uri": "flux://guide"}));
        let text = read["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("# Using Flux") && text.contains("not_installed"), "{text}");
        assert_eq!(ask("resources/read", json!({"uri": "flux://nope"}))["error"]["code"], -32002);
        let prompts = ask("prompts/list", json!({}));
        assert_eq!(prompts["result"]["prompts"][0]["name"], "guide");
        let p = ask("prompts/get", json!({"name": "guide"}));
        assert_eq!(p["result"]["messages"][0]["content"]["text"].as_str(), Some(text));
        assert_eq!(ask("prompts/get", json!({"name": "nope"}))["error"]["code"], -32602);
        for step in ["mix_check apply=true", "add_bus", "action=preset", "auto", "game_audio_check", "loud level"] {
            assert!(text.contains(step), "the guide's mixing and mastering workflow mentions {step}");
        }
    }

    #[test]
    fn the_tools_are_listed_once_each_and_counted_in_the_readme() {
        let list = ask("tools/list", json!({}));
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        assert!(names.contains(&"master") && names.contains(&"mix_check"));
        let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../README.md")).unwrap();
        assert!(readme.contains(&format!("MCP server: {} tools", names.len())), "update the tool count in README.md's badge");
    }
}
