//! The JSON Schema of the `compose` MCP tool. One flat object with an `action` enum (MCP clients handle
//! flat schemas best); each property says which actions use it, and [`super::compose`] rejects
//! properties an action does not take.

use serde_json::{Map, Value, json};

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn track(desc: &str) -> Value {
    json!({"anyOf": [{"type": "integer"}, {"type": "string"}], "description": desc})
}

fn pitch(desc: &str) -> Value {
    json!({"anyOf": [{"type": "integer", "minimum": 0, "maximum": 127}, {"type": "string"}], "description": desc})
}

fn bar(desc: &str) -> Value {
    json!({"type": "integer", "minimum": 1, "description": desc})
}

fn num(desc: &str) -> Value {
    json!({"type": "number", "description": desc})
}

const DESCRIPTION: &str = "Write parts from the chord track and short notations, as one undoable edit. Reads the project; \
the edit it returns replaces what it covers, so running an action again over the same bars gives the same result.\n\
actions:\n\
- melody: notation from `bar`: `pitch:units` separated by spaces, `r:units` rest, `~:units` tie, `|` barline \
(each bar must add up exactly), e.g. \"E4:2 G4:1 B4:1 | A4:3 r:1\". A unit is `unit` beats (default 1, an eighth in x/8). \
Optional `phrase` (true or options) shapes dynamics. Replaces the notes in those bars and reports how the line fits the chords.\n\
- chords: voice-led chords over from_bar..to_bar from the chord track (or `chords`, a chart that is also written to the chord \
track: \"Am | F C | G@0 G/B@2.5 | N.C. | %\", repeated to fill the range). `pattern` is a rhythm, one character per `step` \
beats: x strike, X accented, - hold, . rest, | next bar's pattern (e.g. \"x--x --x-\" with step 0.5). Without a pattern each \
chord is struck once and held.\n\
- bass: a bass line from the chord track (inversions follow slash chords); pattern characters also o/O octave up, 5 fifth.\n\
- arpeggio: broken chords / ostinato: pattern digits 0-9 are voicing tones from the bottom (4 = the bottom an octave up \
with 4 voices), x the whole chord. Default: up and down, step an eighth.\n\
- phrase: re-shape the dynamics of existing notes in from_bar..to_bar (metric accents, an arc to each phrase's high point, \
soft endings). Pass `vel` so re-runs are repeatable.\n\
- swell: an expression CC (default CC1) following the notes of the track (or `source`), replacing that CC in the bars.\n\
- curve: a CC (`cc`) or pitch-bend (`bend`: true, -8192..8191) curve through `points`, replacing that data in its span.\n\
- harmony: with `chords`, write a chart to the chord track from `from_bar`; without, read the chord track back with each \
chord's tones, chord scale, tensions and avoid notes.\n\
- fit: check a track against the chord track: chord tones, tensions, avoid notes and outside notes; `fix` strong|all moves \
flagged notes to the nearest chord tone.";

/// The `inputSchema` of the compose tool.
pub fn compose_schema() -> Value {
    let mut props = Map::new();
    props.extend(object(json!({
        "action": {"enum": ["melody", "chords", "bass", "arpeggio", "phrase", "swell", "curve", "harmony", "fit"]},
        "track": track("track id or name"),
        "bar": bar("melody: the bar the notation starts at"),
        "from_bar": bar("first bar (inclusive). chords/bass/arpeggio/phrase/swell: required; harmony/fit: default 1"),
        "to_bar": bar("last bar (inclusive). chords/bass/arpeggio/phrase/swell: required; harmony: default the chart's length; fit: default the last bar"),
        "notation": {"type": "string", "description": "melody: the line, e.g. \"E4:2 G4:1 B4:1 | A4:3 r:1\""},
        "unit": num("melody: beats per notation unit (default 1; 0.5 in x/8)"),
        "transpose": {"type": "integer", "description": "melody: semitones to shift the notation. fit: semitones the track sounds away from its written notes"},
        "legato": num("melody: length of a note followed by a note, as a fraction of its slot (default 1)"),
        "short": num("melody: length of a note followed by a rest or the end (default 0.85)"),
        "arc": num("melody: velocity points rising to the middle of the line (default 0). phrase: arc to each phrase's high point (default 8)")
    })));
    props.extend(object(json!({
        "phrase": {"anyOf": [{"type": "boolean"}, {"type": "object"}],
            "description": "melody: true, or phrase options {accent, arc, soften, rit, breath, vary, seed, rest, long, breaks}"},
        "chords": {"type": "string", "description": "chords/bass/arpeggio/harmony: a chord chart, one `|` cell per bar (\"Am | F C | G@0 G/B@2.5 | N.C. | %\")"},
        "set_harmony": {"type": "boolean", "description": "chords/bass/arpeggio: also write `chords` to the chord track (default true)"}
    })));
    props.extend(object(json!({
        "voices": {"type": "integer", "minimum": 1, "maximum": 12, "description": "chords/arpeggio: voices in each voicing (default 4)"},
        "low": pitch("chords/arpeggio/bass: lowest pitch (default C3 for chords and arpeggios, C2 for bass)"),
        "high": pitch("chords/arpeggio/bass: highest pitch (default G5 for chords and arpeggios, G3 for bass)"),
        "inversions": {"type": "boolean", "description": "bass: play the slash bass of inversions (default true)"},
        "start": {"anyOf": [pitch("a pitch"), {"type": "array", "items": pitch("a pitch")}],
            "description": "chords/arpeggio: the voicing to lead on from (one pitch per voice); bass: the note before. Default: the track's last chord or note before from_bar"},
        "bass_track": track("chords: a bass track (id or name) to voice above, avoiding parallels with it"),
        "pattern": {"type": "string", "description": "chords/bass/arpeggio: rhythm pattern, one character per step (see the action list)"},
        "step": num("chords/bass/arpeggio: beats per pattern character (default 1, an eighth in x/8; arpeggio half that). swell: beats between CC points (default 0.25). curve: default 0.5"),
        "gate": num("chords/bass/arpeggio: fraction of each note's slot that sounds, (0, 1] (default 1)"),
        "accent": num("chords/bass/arpeggio/phrase: velocity points for the downbeat and for X accents (default 6)")
    })));
    props.extend(object(json!({
        "vel": num("melody/bass: base velocity (default 90); chords/arpeggio: default 80; phrase: base velocity for every note (default: the notes' own)"),
        "replace": {"type": "boolean", "description": "melody/chords/bass/arpeggio: delete the notes already in those bars, keeping keyswitch blips (default true)"},
        "soften": num("phrase: velocity points off each phrase's last note (default 8)"),
        "rit": num("phrase: slow each phrase's last notes by this fraction (default 0; moves notes, so re-runs move them again)"),
        "breath": num("phrase: beats of silence before each new phrase (default 0)"),
        "vary": num("phrase: seeded random velocity variation, +- points (default 2)"),
        "seed": {"type": "integer", "minimum": 0, "description": "phrase: seed of the variation (default 0)"},
        "rest": num("phrase: a gap of this many beats ends a phrase (default 0.5). swell: a gap this long sinks to lo (default 1)"),
        "long": num("phrase: a note this long ends a phrase (default one bar). swell: notes this long swell (default 1.5)"),
        "breaks": {"type": "array", "items": {"type": "integer", "minimum": 1}, "description": "phrase: bars that start a new phrase"}
    })));
    props.extend(object(json!({
        "cc": {"type": "integer", "minimum": 0, "maximum": 127, "description": "swell: controller (default 1); curve: the controller to write"},
        "lo": num("swell: level in rests (default 40)"),
        "hi": num("swell: top level (default 100)"),
        "source": track("swell: the track (id or name) whose notes the swell follows (default the track itself)"),
        "bend": {"type": "boolean", "description": "curve: write pitch bend instead of a CC"},
        "points": {"type": "array", "description": "curve: points in time order",
            "items": {"type": "object", "properties": {
                "bar": bar("bar (1-based); omit to make beat absolute"),
                "beat": num("beats into the bar, or from the song start without bar"),
                "value": num("CC 0-127 or bend -8192..8191")}, "required": ["value"]}},
        "shape": {"enum": ["linear", "smooth", "step"], "description": "curve: how values move between points (default linear)"},
        "fix": {"enum": ["none", "strong", "all"], "description": "fit: move flagged notes on strong beats (strong) or everywhere (all) to the nearest chord tone (default none)"}
    })));
    json!({"type": "object", "properties": props, "required": ["action"]})
}

/// The whole tool definition (`name`, `description`, `inputSchema`) for an MCP `tools/list`.
pub fn compose_tool() -> Value {
    json!({"name": "compose", "description": DESCRIPTION, "inputSchema": compose_schema()})
}
