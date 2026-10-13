//! MCP tools: the agent's whole interface to Flux. Edits go through `Command`s (the same ones the GUI
//! uses); everything else reads state, renders, measures, plays or exports.

use crate::audio::{Clip, Mode};
use crate::state::{letter, Audition, Focus, MainMsg, RenderStatus, Shared, State, Variant};
use anyhow::{anyhow, bail, Context, Result};
use flux_core::commands::Command;
use flux_core::automation::Shape;
use flux_core::clip;
use flux_core::{pitch, CommentOp, Insert, InsertPosition, Instrument, Lane, Mixer, Project, Source, TimeMap, Track, TrackRef};
use flux_engine::{analysis, import, paths, sfizz};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum Content {
    Text(String),
    Png(Vec<u8>),
}

// ---------------------------------------------------------------------------------------------
// schemas

pub(crate) fn track_ref() -> Value {
    json!({"anyOf": [{"type": "integer"}, {"type": "string"}], "description": "track id or name"})
}

fn pitch_s() -> Value {
    json!({"anyOf": [{"type": "integer", "minimum": 0, "maximum": 127}, {"type": "string"}],
           "description": "MIDI number or name, e.g. 60, \"C4\", \"F#2\", \"Bb3\" (C4 = 60)"})
}

fn bar_s(desc: &str) -> Value {
    json!({"type": "integer", "minimum": 1, "description": desc})
}

fn when_props() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("bar".into(), bar_s("bar (1-based); omit to make `beat` absolute from the song start"));
    m.insert("beat".into(), json!({"type": "number", "minimum": 0, "description": "quarter-note beats: offset inside `bar` (0-based), or absolute when no bar"}));
    m
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

fn op(name: &str, desc: &str, mut props: Map<String, Value>, required: &[&str]) -> Value {
    props.insert("op".into(), json!({"const": name}));
    let mut req = vec!["op"];
    req.extend_from_slice(required);
    json!({"type": "object", "description": desc, "properties": props, "required": req})
}

fn props(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn range_props(mut m: Map<String, Value>) -> Map<String, Value> {
    m.insert("from_bar".into(), bar_s("first bar (inclusive); omit for the start"));
    m.insert("to_bar".into(), bar_s("last bar (inclusive); omit for the end"));
    m
}

fn instrument_s() -> Value {
    json!({"oneOf": [
        obj(json!({"kind": {"const": "sfz"}, "path": {"type": "string", "description": "SFZ file (absolute or relative to the project root)"},
                   "init_cc": {"type": "object", "additionalProperties": {"type": "integer"}, "description": "CC values sent at time 0, e.g. {\"1\": 90, \"11\": 127}"}}), &["kind", "path"]),
        obj(json!({"kind": {"const": "vst3"}, "path": {"type": "string", "description": ".vst3 plugin path"},
                   "params": {"type": "object", "additionalProperties": {"type": ["number", "string", "boolean", "object"]},
                              "description": "plugin parameters, applied after the preset. Keys: the parameter's title (any case, or snake_case), short title, or \"#<id>\". Values: a number in the parameter's display units (dB, Hz, %), text the plugin understands (\"250 ms\", \"Sine\"), or {\"normalized\": 0..1}"},
                   "preset": {"type": "string", "description": ".vstpreset file (the plugin's whole state); without one the plugin starts from its default state"},
                   "preset_name": {"type": "string", "description": "what the preset is called (shown to people)"},
                   "class": {"type": "string", "description": "which plugin in a bundle that holds several (name or class id); default: the first one"}}), &["kind", "path"]),
        obj(json!({"kind": {"const": "audio"}, "path": {"type": "string", "description": "an audio file to play from the top of the song: it becomes the track's one clip (place more with add_clip)"}}), &["kind"]),
        obj(json!({"kind": {"const": "none"}}), &["kind"]),
    ]})
}

/// A clip command's fields: `extra` plus where it goes, its gain and fades.
fn clip_props(extra: Value) -> Map<String, Value> {
    let mut m = when_props();
    m.insert("track".into(), track_ref());
    m.insert("gain_db".into(), json!({"type": "number", "minimum": clip::MIN_GAIN_DB, "maximum": clip::MAX_GAIN_DB, "description": "clip gain, before the track's channel strip (default 0)"}));
    m.insert("fade_in".into(), json!({"type": "number", "minimum": 0, "description": "seconds, from silence (linear)"}));
    m.insert("fade_out".into(), json!({"type": "number", "minimum": 0, "description": "seconds, to silence (linear)"}));
    m.extend(props(extra));
    m
}

fn eq_s() -> Value {
    json!({"type": "array", "description": "EQ bands, applied in order (replaces the whole list)", "items": obj(json!({
        "kind": {"enum": ["peak", "low_shelf", "high_shelf", "notch", "tilt", "high_pass", "low_pass"], "description": "peak: a bell at \
hz; low_shelf / high_shelf: everything below / above hz; notch: cuts a narrow band out at hz (a ringing resonance; q sets how narrow, \
db is ignored); tilt: db above hz and -db below through gentle shelves (q ignored), to make the whole sound brighter or darker; \
high_pass / low_pass: removes everything below / above hz at `slope` (db and q ignored): a 20-30 Hz high-pass takes out rumble and \
DC that only cost headroom"},
        "hz": {"type": "number", "minimum": 10, "maximum": 20000},
        "db": {"type": "number", "description": "gain (default 0; notch and passes have none); cuts are safer than boosts, and a few dB is usually enough"},
        "q": {"type": "number", "description": "width: 0.5-0.9 broad (default 0.9), 2-4 narrow (a resonance), 6-12 for a notch; shelves 0.5-1"},
        "slope": {"type": "integer", "enum": [12, 24, 36, 48], "description": "high_pass / low_pass: dB per octave (default 12)"},
        "channel": channel_s()}),
        &["kind", "hz"])})
}

/// Which part of the stereo signal a band works on.
fn channel_s() -> Value {
    json!({"enum": ["stereo", "mid", "side"], "description": "stereo (default): both channels; mid: only what's in the centre \
(L+R: the bass, kick, lead, vocal); side: only what's wide (L-R: reverb, wide pads, doubled guitars). A side high shelf brightens \
the width without touching the lead; a side low cut tightens the low end"})
}

/// Dynamic EQ bands, on a track or the master.
fn dyn_eq_s() -> Value {
    json!({"type": "array", "description": "dynamic EQ bands, after the compressor (replaces the list): each turns its range down \
only while that range is loud, by at most range_db. For a resonance or boomy note that pops out, or harshness that comes and goes, where a \
static cut would thin out the rest of the time", "items": obj(json!({
        "kind": {"enum": ["peak", "low_shelf", "high_shelf"]},
        "hz": {"type": "number", "minimum": 20, "maximum": 20000}, "q": {"type": "number", "description": "default 0.9; 2-4 for a resonance"},
        "threshold_db": {"type": "number", "description": "where it starts acting: dB under the band's loud level when relative (render \
shows it under the track's dyn_eq, or the master_chain's), else dBFS; -3 to -6 catches the loud moments"},
        "relative": {"type": "boolean", "description": "default true"},
        "ratio": {"type": "number", "minimum": 1, "description": "default 3"},
        "range_db": {"type": "number", "minimum": -24, "maximum": 0, "description": "the most it turns the band down (default -6)"},
        "attack_ms": {"type": "number", "description": "default 5"}, "release_ms": {"type": "number", "description": "default 80"},
        "channel": channel_s()}),
        &["kind", "hz", "threshold_db"])})
}

/// A de-esser, on a track or the master.
fn deess_s(after: &str) -> Value {
    json!({"description": format!("de-esser after {after}: turns the band above hz down while it's loud (sibilance, hats and \
cymbals, pick and bow noise, bright leads); null removes it"), "anyOf": [{"type": "null"}, obj(json!({
        "hz": {"type": "number", "minimum": 1000, "maximum": 16000, "description": "where the band starts: 5000-8000 (default 6000); lower for harsh leads"},
        "threshold_db": {"type": "number", "description": "dB under the band's loud level (render shows it under the track's deess, or the master_chain's); lower catches more (default -6)"},
        "range_db": {"type": "number", "minimum": -24, "maximum": 0, "description": "the most it turns the band down (default -6)"}}), &[])]})
}

/// Crossovers that split the master into bands.
fn crossovers_s(min: usize) -> Value {
    json!({"type": "array", "items": {"type": "number", "minimum": 30, "maximum": 16000}, "minItems": min, "maxItems": 3,
        "description": format!("ascending, each at least half an octave (x1.5) above the last: {min}-3 crossovers make {}-4 bands", min + 1)})
}

fn comp_s() -> Value {
    json!({"description": "compressor; null removes it", "anyOf": [{"type": "null"}, obj(json!({
        "threshold_db": {"type": "number", "description": "dBFS, or with relative=true dB under the signal's loud level (95th \
percentile of its short-term RMS; render shows it as a track's comp.loud_db and master_chain.glue.loud_db): -3 to -8 acts on the \
louder moments whatever the level, so it needs no retuning when the mix changes"},
        "relative": {"type": "boolean", "description": "count threshold_db from the loud level (default false)"},
        "ratio": {"type": "number", "minimum": 1, "description": "1.5-2 glue, 3-4 control, 8+ limiting"},
        "attack_ms": {"type": "number", "minimum": 0.05, "maximum": 1000, "description": "default 15; 20-40 lets the hits through (punch), 1-5 catches them"},
        "release_ms": {"type": "number", "minimum": 1, "maximum": 5000, "description": "default 150; with auto_release, the release after sustained compression"},
        "makeup_db": {"type": "number", "description": "gain after it (default 0; the master's loudness drive makes up the master's level anyway)"},
        "knee_db": {"type": "number", "minimum": 0, "maximum": 24, "description": "soft knee: the ratio eases in over this many dB around the threshold (default 0, hard); 6-10 for transparent glue"},
        "detector": {"enum": ["peak", "rms"], "description": "peak (default) catches transients; rms follows loudness, smoother on buses and the master"},
        "sidechain_hpf_hz": {"type": "number", "minimum": 0, "maximum": 1000, "description": "the detector hears the signal high-passed here, so the bass and kick don't pump it: 80-150 on buses and the master's glue (default 0, off)"},
        "lookahead_ms": {"type": "number", "minimum": 0, "maximum": 20, "description": "the audio runs this far behind the detector, so even a fast transient is caught: 1-5 on drums and plucks (default 0)"},
        "auto_release": {"type": "boolean", "description": "program-dependent release: fast after short peaks, release_ms after sustained compression (no pumping; default false)"},
        "mix": {"type": "number", "minimum": 0, "maximum": 1, "description": "parallel compression: the compressed signal's share (default 1); 0.3-0.6 adds density and keeps the attacks"}}),
        &["threshold_db", "ratio"])]})
}

/// The built-in reverb's settings: the master's shared reverb, and a strip's (`extra` adds the strip's mix).
fn reverb_props(extra: Value) -> Map<String, Value> {
    let mut m = props(json!({
        "algorithm": {"enum": ["hall", "room", "plate", "classic"], "description": "hall: large, slow build, smooth long tail; room: small and \
dense, short; plate: bright and dense, no early reflections (vocals, leads, snares); classic: the Freeverb-style reverb of older \
projects (metallic on long tails). Left out: classic"},
        "room_size": {"type": "number", "minimum": 0, "maximum": 1, "description": "how big the space is (sets the decay unless decay_s does; default 0.7)"},
        "damping": {"type": "number", "minimum": 0, "maximum": 1, "description": "how much faster the highs die away than the lows (default 0.5)"},
        "predelay_ms": {"type": "number", "minimum": 0, "maximum": 500, "description": "gap before the reverb starts (keeps the attacks clear); 10-40 (default 20)"},
        "hpf_hz": {"type": "number", "description": "high-pass of the reverb (keeps the low end clean); 150-300 (default 220)"},
        "lpf_hz": {"type": "number", "description": "low-pass of the reverb (darker, further away); 5000-9000 (default 7500)"},
        "return_db": {"type": "number", "description": "level of the reverb's output (default 0 = as is)"},
        "decay_s": {"type": "number", "minimum": 0, "maximum": 12, "description": "decay time (RT60, seconds) in place of the one room_size gives; 0 = from room_size (default)"},
        "width": {"type": "number", "minimum": 0, "maximum": 1, "description": "stereo width of the reverb (default 1)"},
    }));
    m.extend(props(extra));
    m
}

fn mixer_s() -> Value {
    obj(
        json!({
            "gain_db": {"type": "number", "description": "fader (pre-drive)"},
            "pan": {"type": "number", "minimum": -1, "maximum": 1},
            "width": {"type": "number", "minimum": 0, "maximum": 2, "description": "stereo width (0 = mono, 1 = as is)"},
            "mute": {"type": "boolean"}, "solo": {"type": "boolean"},
            "hpf_hz": {"type": "number", "description": "high-pass, 0 = off"},
            "lpf_hz": {"type": "number", "description": "low-pass, 0 = off"},
            "hpf_slope": {"type": "integer", "enum": [12, 24, 36, 48], "description": "high-pass slope, dB/octave (default 12; 24-48 to take out rumble without thinning what's above)"},
            "lpf_slope": {"type": "integer", "enum": [12, 24, 36, 48], "description": "low-pass slope, dB/octave (default 12)"},
            "mono_below_hz": {"type": "number", "minimum": 0, "maximum": 1000, "description": "make the track mono below this frequency (its side high-passed at 24 dB/octave, before width and pan), 0 = off; 100-200 Hz folds a wide low end to mono without losing level"},
            "eq": eq_s(),
            "drive_db": {"type": "number", "description": "tanh saturation amount, 0 = off"},
            "amp": {"type": ["object", "null"], "description": "guitar amp after drive (null = off): gain_db (15 crunch, 30 high gain), tight_hz, mid_hz, mid_db, cab_low_hz, cab_high_hz, scoop_db, presence_db, level_db; omitted fields use defaults"},
            "chorus": {"description": "modulated-delay chorus; null removes it", "anyOf": [{"type": "null"}, obj(json!({
                "rate_hz": {"type": "number"}, "depth": {"type": "number", "minimum": 0, "maximum": 1},
                "mix": {"type": "number", "minimum": 0, "maximum": 1}, "centre_delay_ms": {"type": "number", "description": "default 7"}}),
                &["rate_hz", "depth", "mix"])]},
            "bitcrush_bits": {"type": "number", "description": "bit-depth reduction, 0 = off"},
            "clip_db": {"description": "hard clip level in dBFS; null removes it", "anyOf": [{"type": "null"}, {"type": "number"}]},
            "post_gain_db": {"type": "number", "description": "gain after drive / chorus / bitcrush / clip"},
            "gate": {"description": "noise gate; null removes it", "anyOf": [{"type": "null"}, obj(json!({
                "threshold_db": {"type": "number"}, "attack_ms": {"type": "number"}, "release_ms": {"type": "number"}}), &["threshold_db"])]},
            "comp": comp_s(),
            "delay": {"description": "feedback delay; null removes it", "anyOf": [{"type": "null"}, obj(json!({
                "seconds": {"type": "number"}, "feedback": {"type": "number"}, "mix": {"type": "number"}}), &["seconds", "feedback", "mix"])]},
            "duck": {"description": "sidechain ducking keyed by another track's note-ons; null removes it", "anyOf": [{"type": "null"}, obj(json!({
                "source_track": track_ref(), "depth_db": {"type": "number"}, "release_ms": {"type": "number"}}), &["source_track", "depth_db", "release_ms"])]},
            "reverb_send": {"type": "number", "minimum": 0, "maximum": 1, "description": "send to the master's shared reverb"},
            "dyn_eq": dyn_eq_s(),
            "deess": deess_s("the dynamic EQ"),
            "reverb": {"description": "a reverb in the strip, after the delay; null removes it. On an aux return (a bus that tracks send to) set \
mix 1 so the bus returns only the reverb; on a track, mix 0.1-0.3. Settings left out take their defaults", "anyOf": [{"type": "null"}, obj(Value::Object(reverb_props(json!({
                "mix": {"type": "number", "minimum": 0, "maximum": 1, "description": "the reverb's share of the output: 1 = only the reverb (default), 0.2 = mostly dry"}}))),
                &[])]},
            "output": {"description": "the bus this track plays into instead of the master (a group: drums, strings...); null = the master. \
add_bus makes a bus and routes tracks to it in one go", "anyOf": [{"type": "null"}, track_ref()]},
            "sends": {"type": "array", "description": "post-fader sends to aux returns (buses with a reverb or delay at mix 1), besides \
reverb_send; replaces the list ([] removes them)", "items": obj(json!({"to": track_ref(),
                "level": {"type": "number", "minimum": 0, "maximum": 1, "description": "0.1-0.3 usual"}}), &["to", "level"])},
        }),
        &[],
    )
}

fn master_s() -> Value {
    obj(
        json!({
            "glue": comp_s(),
            "eq": eq_s(),
            "eq_phase": {"enum": ["minimum", "linear"], "description": "the master EQ's filters: minimum (default) shifts the phase a \
little around each band, as an analog EQ does; linear keeps attacks exactly as mixed, for broad tonal moves and mid/side bands, at about \
0.1 s more latency live and a little pre-ringing before sharp attacks where a band is narrow. High-passes always run in minimum phase \
(linear phase can't follow them that low); keep narrow bands under 40 Hz in minimum phase for the same reason"},
            "dyn_eq": dyn_eq_s(),
            "deess": deess_s("the master's dynamic EQ"),
            "target_lufs": {"type": "number", "description": "integrated loudness target (default -15, Roblox's); the master drives the mix there"},
            "ceiling_dbtp": {"type": "number", "maximum": 0, "description": "true-peak ceiling (default -1)"},
            "reverb": {"description": "the shared reverb the tracks' reverb_send feeds. Replaces the whole reverb: settings left \
out take their defaults, not the current values", "type": "object", "properties": reverb_props(json!({}))},
            "edge_fade_ms": {"type": "number", "description": "fade in/out for one-shot (non-loop) exports"},
            "tail_s": {"type": "number", "description": "seconds rendered past the last bar (reverb / release tails)"},
            "limiter": {"type": "object", "description": "the final limiter after the loudness drive, which holds the true peak under the \
ceiling. Replaces the whole limiter: fields left out take their defaults, not the current values", "properties": {
                "lookahead_ms": {"type": "number", "minimum": 0.5, "maximum": 20, "description": "how far ahead it starts turning down for a peak (default 5)"},
                "release_ms": {"type": "number", "minimum": 0, "maximum": 2000, "description": "how slowly the gain recovers (default 80; 150-300 smoother on sustained music, 30-60 punchier)"},
                "auto_release": {"type": "boolean", "description": "short peaks recover fast, sustained limiting at release_ms (default true)"},
                "clip_db": {"type": "number", "minimum": 0, "maximum": 6, "description": "the most a soft-clipping stage takes off the sharpest peaks before the lookahead stage (default 1; 1.5-2.5 louder and denser drums, 0 off)"}}},
            "multiband": {"description": "multiband compressor after the master EQ, dynamic EQ and de-esser: the master split at the \
crossovers, each band compressed on its own, so a region that flares up (boomy bass notes, harsh peaks) is held back only then. null \
removes it. A good start: crossovers [150, 2500, 8000], thresholds -3 to -4, ratios 1.6-2", "anyOf": [{"type": "null"}, obj(json!({
                "crossovers_hz": crossovers_s(1),
                "bands": {"type": "array", "description": "one per band, low to high", "items": obj(json!({
                    "threshold_db": {"type": "number", "description": "dB under the band's loud level (render shows it in master_chain): -3 to -6"},
                    "ratio": {"type": "number", "minimum": 1}, "attack_ms": {"type": "number", "description": "default 15; slower in the lows (30), faster in the highs (5)"},
                    "release_ms": {"type": "number", "description": "default 150"},
                    "gain_db": {"type": "number", "minimum": -12, "maximum": 12, "description": "the band's level after it (default 0)"},
                    "knee_db": {"type": "number", "minimum": 0, "maximum": 24, "description": "soft knee: the ratio eases in over this many dB (default 6; 10-12 more transparent, 0 hard)"},
                    "mix": {"type": "number", "minimum": 0, "maximum": 1, "description": "parallel compression: the compressed band's share (default 1); 0.4-0.6 adds density and keeps the attacks"},
                    "channel": channel_s()}),
                    &["threshold_db", "ratio"])}}), &["crossovers_hz", "bands"])]},
            "exciter": {"description": "multiband exciter after the multiband compressor: each band saturated on its own (4x \
oversampled) and blended in, adding harmonics that bring out a dull or thin range without raising its noise and harshness as an EQ \
boost would. null removes it. A gentle start: {} (its defaults: the top two bands at mix 0.15 and 0.25 add air and presence)",
                "anyOf": [{"type": "null"}, obj(json!({
                "crossovers_hz": {"type": "array", "items": {"type": "number", "minimum": 30, "maximum": 16000}, "maxItems": 3,
                    "description": "ascending, each at least half an octave (x1.5) above the last: 0-3 crossovers make 1-4 bands (default [150, 2500, 8000])"},
                "bands": {"type": "array", "description": "one per band, low to high", "items": obj(json!({
                    "drive_db": {"type": "number", "minimum": -12, "maximum": 24, "description": "how hard the band's loud passages drive \
the curve, counted from the band's loud level (render shows it in master_chain): 0 rounds only the peaks, 6 (default) is warm, 12 clearly saturated"},
                    "mix": {"type": "number", "minimum": 0, "maximum": 1, "description": "the saturated band's share (0 = untouched): 0.1-0.3 is usually plenty"},
                    "mode": {"enum": ["tape", "tube"], "description": "tape (default): odd harmonics, clear and bright, for presence and \
air; tube: even and odd harmonics, warmer and fuller, for a thin low-mid or bass that needs to be heard on small speakers"}}),
                    &["mix"])}}), &[])]},
            "imager": {"description": "multiband stereo imager after the exciter: each band's width on its own (the side signal \
scaled), so the lows can be narrowed and the highs widened in one place. null removes it. E.g. crossovers [150, 2500, 8000], widths \
[0.6, 1.0, 1.1, 1.2]", "anyOf": [{"type": "null"}, obj(json!({
                "crossovers_hz": crossovers_s(1),
                "widths": {"type": "array", "items": {"type": "number", "minimum": 0, "maximum": 2}, "description": "one per band, low to \
high: 0 mono, 1 as mixed, up to 2. Widening past 1.3 thins the mono fold-down"}}), &["crossovers_hz", "widths"])]},
            "mono_below_hz": {"type": "number", "minimum": 0, "maximum": 500, "description": "the master is mono below this (after the imager), 0 = off: 100-150 keeps a wide low end from wandering and wasting headroom"},
            "width": {"type": "number", "minimum": 0, "maximum": 2, "description": "the master's stereo width (0 mono, 1 as mixed, default)"},
        }),
        &[],
    )
}

fn command_s() -> Value {
    let note = obj(
        Value::Object({
            let mut m = when_props();
            m.insert("dur".into(), json!({"type": "number", "exclusiveMinimum": 0, "description": "length in beats"}));
            m.insert("pitch".into(), pitch_s());
            m.insert("vel".into(), json!({"type": "integer", "minimum": 1, "maximum": 127, "description": "default 90"}));
            m
        }),
        &["dur", "pitch"],
    );
    let note_key = obj(
        Value::Object({
            let mut m = when_props();
            m.insert("pitch".into(), pitch_s());
            m
        }),
        &["pitch"],
    );
    let cc_point = obj(
        Value::Object({
            let mut m = when_props();
            m.insert("value".into(), json!({"type": "integer", "minimum": 0, "maximum": 127}));
            m
        }),
        &["value"],
    );
    let bend_point = obj(
        Value::Object({
            let mut m = when_props();
            m.insert("value".into(), json!({"type": "integer", "minimum": -8192, "maximum": 8191, "description": "0 = centre"}));
            m
        }),
        &["value"],
    );
    let auto_point = obj(
        Value::Object({
            let mut m = when_props();
            m.insert("value".into(), json!({"type": "number", "description": "volume: dB on top of the fader (-90 = silent, max +12); pan: -1 left to 1 right; reverb_send: 0-1; param: the parameter's display units (dB, Hz, %), or 0-1 with normalized"}));
            m.insert("shape".into(), json!({"enum": ["linear", "smooth", "hold"],
                "description": "how the value gets to the next point: linear (default), smooth (an S-curve, for swells and fades) or hold (stays, then jumps there)"}));
            m
        }),
        &["value"],
    );
    let auto_target = || {
        json!({"enum": ["volume", "pan", "reverb_send", "param"],
            "description": "volume, pan and reverb_send move the track's mixer (volume in dB on top of its fader; pan and reverb_send replace the mixer's while the lane exists); param moves a VST3 parameter"})
    };
    let t = || track_ref();
    json!({"oneOf": [
        op("set_project", "project properties (bpm replaces the tempo map with one tempo)", props(json!({
            "name": {"type": "string"}, "key": {"type": "string"}, "bars": {"type": "integer", "minimum": 1},
            "looped": {"type": "boolean", "description": "render as a seamless loop"},
            "time_signature": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2},
            "bpm": {"type": "number"}, "notes": {"type": "string", "description": "free-text project notes"}})), &[]),
        op("set_tempo", "stepped tempo map; each point starts at a bar", props(json!({
            "points": {"type": "array", "items": obj(json!({"bar": {"type": "integer", "minimum": 1}, "bpm": {"type": "number"}}), &["bar", "bpm"])}})), &["points"]),
        op("set_sections", "named sections (arrangement markers); replaces all", props(json!({
            "sections": {"type": "array", "items": obj(json!({"name": {"type": "string"}, "start_bar": {"type": "integer", "minimum": 1}, "bars": {"type": "integer", "minimum": 1}}), &["name", "start_bar", "bars"])}})), &["sections"]),
        op("set_harmony", "the chord track (what the harmony is; parts voice it); replaces all. Each chord holds until the next", props(json!({
            "chords": {"type": "array", "items": obj(json!({"bar": {"type": "integer", "minimum": 1},
                "beat": {"type": "number", "minimum": 0, "description": "offset into the bar in beats (default 0)"},
                "chord": {"type": "string", "description": "chord symbol: Am, F#m7b5, Cmaj7, Dsus4, G7/B, Bb/D; N.C. for none"}}), &["bar", "chord"])}})), &["chords"]),
        op("add_track", "new track (returns its id)", props(json!({"name": {"type": "string"}, "instrument": instrument_s(),
            "color": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3}})), &["name"]),
        op("remove_track", "delete a track", props(json!({"track": t()})), &["track"]),
        op("update_track", "rename, recolour, change the instrument or the stem layer", props(json!({"track": t(), "name": {"type": "string"},
            "instrument": instrument_s(), "color": {"type": "array", "items": {"type": "integer"}},
            "layer": {"type": "string", "description": "game stem layer this track belongs to (export layers=true): e.g. base, melody, percussion; default base"}})), &["track"]),
        op("set_mixer", "change channel-strip settings; only the given fields change. The strip runs: pre inserts, HPF/LPF, EQ, gain, \
drive, amp, chorus, bitcrush, clip, post gain, gate, compressor, dynamic EQ, de-esser, delay, reverb, post inserts, duck, mono below, \
width/pan, then the sends and its output (the master, or a bus)", props(json!({"track": t(), "mixer": mixer_s()})), &["track", "mixer"]),
        op("add_bus", "make a bus (returns track_id): a track without an instrument that other tracks play into. A group bus (tracks) \
gets the tracks' output, so one fader, EQ or compressor treats them together; an aux return (sends) gets a send from each track and \
usually carries mixer.reverb with mix 1 (or a delay, or a reverb insert): one shared space instead of a reverb per track. Change it \
later with set_mixer like any track", props(json!({
            "name": {"type": "string"},
            "tracks": {"type": "array", "items": track_ref(), "description": "tracks whose output goes to the bus (instead of the master)"},
            "sends": {"type": "array", "description": "tracks that send to the bus (they still play into their own output)", "items": obj(json!({
                "track": track_ref(), "level": {"type": "number", "minimum": 0, "maximum": 1, "description": "0.1-0.3 usual"}}), &["track", "level"])},
            "mixer": mixer_s(),
            "color": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3}})), &["name"]),
        op("set_master", "change the master bus; only the given fields change", props(json!({"master": master_s()})), &["master"]),
        op("add_insert", "put a VST3 effect in an insert slot (returns insert_id): on a track (pre = before its filters/EQ, post = after its delay, before pan and sends), on the master (after the reverb return joins, before glue) or on the reverb return (replaces the built-in reverb: the sends go through the chain; return_db still applies). Set it up with params, or load presets with the plugins tool", props(json!({
            "track": t(), "bus": {"enum": ["master", "reverb"], "description": "instead of track"},
            "path": {"type": "string", "description": ".vst3 effect (plugins scan kind=effect lists them)"},
            "class": {"type": "string", "description": "which plugin in a bundle that holds several"},
            "params": {"type": "object", "description": "as a vst3 instrument's params, applied after the preset"},
            "preset": {"type": "string", "description": ".vstpreset file with the plugin's whole state"},
            "preset_name": {"type": "string"},
            "bypass": {"type": "boolean"},
            "position": {"enum": ["pre", "post"], "description": "track inserts: default post"},
            "index": {"type": "integer", "minimum": 0, "description": "slot in its group (default: last)"}})), &["path"]),
        op("update_insert", "change an insert; only the given fields change", props(json!({
            "insert": {"type": "integer", "description": "insert id"},
            "params": {"type": "object", "description": "merged into its params; a null value removes one"},
            "clear_params": {"type": "boolean", "description": "drop its params first"},
            "preset": {"type": ["string", "null"], "description": ".vstpreset file; null returns to the plugin's default state"},
            "preset_name": {"type": "string"}, "bypass": {"type": "boolean"},
            "position": {"enum": ["pre", "post"]}})), &["insert"]),
        op("remove_insert", "delete an insert", props(json!({"insert": {"type": "integer"}})), &["insert"]),
        op("move_insert", "reorder an insert within its chain", props(json!({"insert": {"type": "integer"},
            "index": {"type": "integer", "minimum": 0, "description": "new 0-based slot in the chain"}})), &["insert", "index"]),
        op("add_notes", "add notes", props(json!({"track": t(), "notes": {"type": "array", "items": note}})), &["track", "notes"]),
        op("delete_notes", "delete exact notes (start within 0.001 beat + pitch); batch with add_notes to move or resize",
            props(json!({"track": t(), "notes": {"type": "array", "items": note_key}})), &["track", "notes"]),
        op("remove_notes", "delete all notes in a bar range (optionally one pitch)",
            range_props(props(json!({"track": t(), "pitch": pitch_s()}))), &["track"]),
        op("transpose_notes", "shift pitches", range_props(props(json!({"track": t(), "semitones": {"type": "integer"}}))), &["track", "semitones"]),
        op("scale_velocity", "multiply velocities", range_props(props(json!({"track": t(), "factor": {"type": "number"}}))), &["track", "factor"]),
        op("quantize", "snap note starts toward a grid", range_props(props(json!({"track": t(),
            "grid": {"type": "number", "description": "beats: 1 = quarter, 0.5 = eighth, 0.25 = sixteenth, 1/3 = eighth triplet"},
            "strength": {"type": "number", "minimum": 0, "maximum": 1, "description": "default 1"},
            "ends": {"type": "boolean", "description": "also snap note ends"}}))), &["track", "grid"]),
        op("humanize", "deterministic random timing/velocity/length variation", range_props(props(json!({"track": t(),
            "timing_ms": {"type": "number", "description": "uniform +- jitter, e.g. 8-20"},
            "velocity": {"type": "integer", "description": "+- velocity jitter"},
            "length": {"type": "number", "description": "+- fraction of each note's length"},
            "seed": {"type": "integer"}}))), &["track"]),
        op("add_cc", "MIDI CC automation points (1 = mod wheel / dynamics, 11 = expression, 64 = sustain pedal, 7 = volume)",
            props(json!({"track": t(), "cc": {"type": "integer", "minimum": 0, "maximum": 127}, "points": {"type": "array", "items": cc_point}})), &["track", "cc", "points"]),
        op("clear_cc", "delete CC points (one controller or all) in a bar range",
            range_props(props(json!({"track": t(), "cc": {"type": "integer"}}))), &["track"]),
        op("add_bend", "pitch-bend points", props(json!({"track": t(), "points": {"type": "array", "items": bend_point}})), &["track", "points"]),
        op("add_automation", "write an automation lane: a track's volume, pan or reverb send over time, or a VST3 parameter of its instrument \
(no insert) or of one of its inserts; with bus \"master\", a parameter of an insert on the master or the reverb return. The points replace the \
lane from the first given beat to the last (points elsewhere stay). Before the first point the lane holds the first value, after the last the \
last value; two points at one beat make a jump. Jumps play as 5 ms ramps (no clicks), and a looped song's lanes run from its end back into \
its start, so end a loop where it starts for a seamless seam. Heard live while playing; rigs' stages can't be automated (use an insert)",
            props(json!({
                "track": t(), "bus": {"enum": ["master"], "description": "instead of track: the master's lanes (parameters of master and reverb-return inserts)"},
                "target": auto_target(),
                "insert": {"type": "integer", "description": "param: the insert whose parameter moves (omit for the track's VST3 instrument)"},
                "param": {"type": "string", "description": "param: the parameter's title (any case, or snake_case), short title or \"#<id>\" (plugins params lists them)"},
                "normalized": {"type": "boolean", "description": "param: values are 0-1 of the parameter's range (needed when a plugin can't read its own display values back); a lane is all one or the other"},
                "replace": {"type": "boolean", "description": "the points become the whole lane (default: they replace only the span they cover); with no points, the lane is deleted"},
                "points": {"type": "array", "items": auto_point}})), &["target", "points"]),
        op("clear_automation", "delete automation: the points in a bar range, or whole lanes without one; target, insert and param narrow it to some lanes",
            range_props(props(json!({"track": t(), "bus": {"enum": ["master"], "description": "instead of track: the master's lanes"},
                "target": auto_target(), "insert": {"type": "integer"}, "param": {"type": "string"}}))), &[]),
        op("copy_bars", "copy a bar range to another bar (one track, or every track and the master): notes, CCs, bends and audio clips join what is there; automation replaces the lanes over the copied span",
            props(json!({"track": t(),
            "from_bar": {"type": "integer", "minimum": 1}, "bars": {"type": "integer", "minimum": 1}, "to_bar": {"type": "integer", "minimum": 1}})), &["from_bar", "bars", "to_bar"]),
        op("clear_bars", "delete notes, CCs, bends, audio and automation points in a bar range (one track, or every track and the master); clips across its edges are cut there", props(json!({"track": t(),
            "from_bar": {"type": "integer", "minimum": 1}, "bars": {"type": "integer", "minimum": 1}})), &["from_bar", "bars"]),
        op("add_clip", "place an audio file on an audio track (returns clip_id), anywhere and as often as you like; clips that overlap play \
together (fade one out over the other's fade in to crossfade). WAV, FLAC or Ogg Vorbis at any sample rate (resampled to the project's), played \
at its own speed (not stretched to the tempo). A track without an instrument becomes an audio track", clip_props(json!({
            "path": {"type": "string", "description": "audio file (absolute or relative to the project root)"},
            "offset": {"type": "number", "minimum": 0, "description": "seconds into the file where it starts playing (default 0)"},
            "length": {"type": "number", "exclusiveMinimum": 0, "description": "seconds it plays (default: to the end of the file)"}})), &["track", "path"]),
        op("update_clip", "change a clip; only the given fields change. bar/beat move the whole clip; with trim=true they move only its start, \
and the audio stays where it was (its front is cut off, or more of the file uncovered). track moves it to another audio track", clip_props(json!({
            "clip": {"type": "integer", "description": "clip id (get_notes and get_project list them)"},
            "trim": {"type": "boolean", "description": "bar/beat move only the clip's start (not with offset)"},
            "path": {"type": "string", "description": "play another file"},
            "offset": {"type": "number", "minimum": 0, "description": "seconds into the file where it starts playing"},
            "length": {"type": ["number", "null"], "exclusiveMinimum": 0, "description": "seconds it plays; null: to the end of the file"}})), &["clip"]),
        op("remove_clip", "delete a clip", props(json!({"clip": {"type": "integer"}})), &["clip"]),
        op("split_clip", "cut a clip in two (returns clip_id: the part after the cut); the audio plays on exactly as before", props(json!({
            "clip": {"type": "integer"},
            "bar": bar_s("bar of the cut (1-based); omit to make `beat` absolute"),
            "beat": {"type": "number", "minimum": 0, "description": "the cut: offset inside `bar`, or absolute"}})), &["clip"]),
    ]})
}

pub(crate) fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": desc, "inputSchema": {"type": "object", "properties": props, "required": required}})
}

pub fn list() -> Vec<Value> {
    vec![
        tool("get_project", "Overview of the open project: tempo, sections, master settings, every track (instrument, note count, \
pitch range, bars used, non-default mixer settings), undo state and whether the last render is stale. detail=full returns the entire project JSON.",
            json!({"detail": {"enum": ["summary", "full"]}}), &[]),
        tool("get_notes", "List a track's notes bar by bar as `beat pitch dur vel` (beat = 0-based offset inside the bar), plus its CCs and automation lanes.",
            json!({"track": track_ref(), "from_bar": bar_s("default 1"), "to_bar": bar_s("default last bar"), "include_cc": {"type": "boolean", "description": "default true"}}), &["track"]),
        tool("edit", "Apply a batch of edit commands atomically (all succeed or nothing changes; one undo step). Every change to the \
project goes through here: tracks, instruments, notes, CCs, automation, tempo, sections, mixer and master.",
            json!({"commands": {"type": "array", "items": command_s(), "minItems": 1}}), &["commands"]),
        tool("undo", "Undo the last change(s), whoever made them.", json!({"steps": {"type": "integer", "minimum": 1}}), &[]),
        tool("redo", "Redo undone change(s).", json!({"steps": {"type": "integer", "minimum": 1}}), &[]),
        tool("new_project", "Start an empty project (the previous one stays reachable with undo).", json!({
            "name": {"type": "string"}, "bpm": {"type": "number"}, "bars": {"type": "integer", "minimum": 1},
            "time_signature": {"type": "array", "items": {"type": "integer"}}, "looped": {"type": "boolean", "description": "default true"},
            "key": {"type": "string"}}), &["name", "bpm", "bars"]),
        tool("open", "Open a Flux project (.flux.json), import a pipeline song (its <title>.json beside the .mid and stems/) or a MIDI file (.mid).",
            json!({"path": {"type": "string"}}), &["path"]),
        tool("save", "Save the project (default: where it was opened, else <name>.flux.json in the projects folder).", json!({"path": {"type": "string"}}), &[]),
        tool("list_instruments", "Find instruments on disk: SFZ programs (instruments/, build/sfz/, sfz/), VST3 plugins and guitar/bass rigs \
(sfz/rigs/*.rig.json: a sampled guitar or bass into amp captures and cabinets; play one by giving its path as a vst3 instrument). SFZ \
fragments (files that only work #included by a program) are left out unless programs_only=false. details=true adds each SFZ's regions, \
keys, keyswitches, velocity layers and missing samples, and each rig's description, keyswitches and chain (read from the files; no audio \
is played).", json!({
            "query": {"type": "string", "description": "case-insensitive substring of the path"},
            "kind": {"enum": ["sfz", "vst3", "rig", "any"]}, "limit": {"type": "integer"},
            "programs_only": {"type": "boolean", "description": "leave out SFZ fragments (default true)"},
            "details": {"type": "boolean", "description": "one summary per SFZ instead of the bare path (default false)"}}), &[]),
        tool("probe_instrument", "Describe an SFZ instrument from its files: keys, keyswitches and their articulations, velocity layers, round \
robins, release samples, controllers and what they do, samples, problems. Unless render=false it also plays every key offline and reports \
which keys really sound, their levels by octave and the region count sfizz loaded. Use it before writing a part, to stay inside the \
instrument's real range and use its articulations.", json!({
            "path": {"type": "string"}, "velocity": {"type": "integer", "minimum": 1, "maximum": 127},
            "cc": {"type": "object", "additionalProperties": {"type": "integer"}, "description": "CCs sent first, e.g. {\"1\": 100}"},
            "from": pitch_s(), "to": pitch_s(),
            "render": {"type": "boolean", "description": "also play every key through sfizz (default true; false answers at once)"},
            "format": {"enum": ["text", "json"], "description": "description as a few lines of text plus keyswitches and controllers as JSON \
(default), or everything as JSON"}}), &["path"]),
        tool("sfz_sets", "The SFZ wrapper sets (sfz/sets/<set>.toml -> build/sfz/<set>/, the instruments songs use): check which wrappers \
are up to date, stale or broken and which file to load; build the stale ones (incremental; files edited by hand are kept unless force); or \
list files in the output folders that no set makes.", json!({
            "action": {"enum": ["check", "build", "untracked"]},
            "sets": {"type": "array", "items": {"type": "string"}, "description": "set names (default: every set)"},
            "regenerate": {"type": "boolean", "description": "check: also generate each wrapper and compare it with the file on disk"},
            "force": {"type": "boolean", "description": "build: regenerate everything, replacing hand edits"},
            "dry_run": {"type": "boolean", "description": "build: report what would be done without writing"},
            "only": {"type": "array", "items": {"type": "string"}, "description": "build: only these instruments"}}), &["action"]),
        tool("sfz_derive", "Make a fixed copy of an SFZ instrument (the source is never changed, and nothing is ever written in instruments/): \
tighten (short release, no release samples: tight palm mutes), retune (measure each sample's pitch from its harmonics and correct its tune; noise and release samples are left alone) or even (bring \
every key to the median level of its velocity layer, measured through sfizz). Returns the files written and a summary.", json!({
            "tool": {"enum": ["tighten", "retune", "even"]},
            "source": {"type": "string", "description": "SFZ file (absolute or relative to the project root)"},
            "out": {"type": "string", "description": "where the copy goes (default sfz/derived/<library>/<name>_<tool>.sfz; a copy of a copy goes next to it)"},
            "sample_paths": {"enum": ["relative", "absolute"], "description": "how the copy names the original samples (default relative)"},
            "release_s": {"type": "number", "minimum": 0, "description": "tighten (required): the new release time; even: also set every ampeg_release"},
            "note": {"type": "string", "description": "tighten: added to the provenance comment"},
            "keys": {"type": "array", "items": pitch_s(), "minItems": 2, "maxItems": 2, "description": "retune: keep only regions whose lowest key is in [lo, hi]"},
            "max_cents": {"type": "number", "description": "retune: larger corrections are not trusted (default 60)"},
            "add_to_tune": {"type": "boolean", "description": "retune: add the correction to the existing tune instead of replacing it"},
            "base": {"type": "string", "description": "retune: the source is a fragment whose samples are relative to this folder; the copy keeps its sample paths"},
            "velocities": {"type": "array", "items": {"type": "integer", "minimum": 1, "maximum": 127}, "description": "even: one velocity per layer, softest first (default [50, 120])"},
            "window_s": {"type": "number", "description": "even: seconds measured from each note-on (default 0.6)"},
            "max_change_db": {"type": "number", "description": "even: largest volume change either way (default 12)"},
            "reset_between_notes": {"type": "boolean", "description": "even: cut all sound before each measurement (default true)"}}), &["tool", "source"]),
        tool("render", "Render the project (instruments, channel strips, reverb, master chain with loudness drive and true-peak limiter). \
Returns loudness, true peak, loop-seam error and per-track status. Waits for completion by default.", json!({
            "wait": {"type": "boolean"}, "timeout_s": {"type": "number"},
            "stems_dir": {"type": "string", "description": "also write each processed track as a float WAV here (parts whose license forbids exposing them, e.g. Omnisphere, are skipped)"},
            "layers": {"type": "boolean", "description": "also render one stem per track layer (they sum to the master)"}}), &[]),
        tool("analyze", "Measurements of the last render: loudness/peaks, per-section loudness, per-track RMS per section, octave-band \
balance, per-track brightness/attack (hi3k_db, attack_db) and (bars=true) per-bar loudness, plus automatic warnings \
(including harsh or sharp-attack tracks, which this project's listener dislikes).", json!({
            "sections": {"type": "boolean"}, "balance": {"type": "boolean"}, "bands": {"type": "boolean"}, "bars": {"type": "boolean"}}), &[]),
        tool("export", "Write the master: wav16 (dithered), wav32 (float) or flac (16-bit, dithered). Re-renders first if the render is stale. \
Reports file size and Roblox upload limits. layers=true also writes one file per track layer (base, melody, percussion... set with \
update_track layer), which sum to the master, plus a <name>.layers.json manifest (tempo, loop length, section times) for the game. A layer carried by an instrument whose license forbids exposing it (Spectrasonics: Omnisphere, Keyscape, Trilian) is refused: give that layer other instruments too.", json!({
            "path": {"type": "string"}, "format": {"enum": ["wav16", "wav32", "flac"]}, "render_if_stale": {"type": "boolean"},
            "layers": {"type": "boolean"},
            "dither": {"enum": ["tpdf", "shaped"], "description": "how 16-bit files are dithered: tpdf (default) is flat noise at \
-96 dBFS; shaped moves that noise out of 1-6 kHz, where hearing is sharpest, into the top octave (about 20 dB quieter where it \
counts), for songs with quiet passages, fade-outs or long reverb tails. Loudness and true peak don't change; a FLAC grows about 5-7%"}}), &[]),
        tool("transport", "Play on this computer's speakers (for the human), stop, seek, or get the playhead. mode=live plays the current \
project straight away through the live engine (edits are heard while playing); mode=render plays the last offline render (the exact master).", json!({
            "action": {"enum": ["play", "stop", "seek", "status"]},
            "mode": {"enum": ["live", "render"], "description": "switch playback mode (default: keep the current one, initially live)"},
            "bar": {"type": "number", "description": "seek target; 12.5 = halfway through bar 12"}}), &["action"]),
        tool("comments", "Timeline comments: notes pinned to a bar (range) and optionally a track. The human writes them in the GUI \
(\"harp too loud here\"); read them with get_project or list, answer with reply, and resolve them when done (with a reply saying what you changed). \
Comments are not part of undo.", json!({
            "action": {"enum": ["list", "add", "reply", "resolve", "reopen", "edit", "remove"]},
            "id": {"type": "integer"}, "bar": {"type": "integer", "minimum": 1}, "beat": {"type": "number"},
            "to_bar": {"type": "integer", "minimum": 1}, "track": track_ref(), "text": {"type": "string"},
            "reply": {"type": "string", "description": "resolve: closing reply"},
            "include_resolved": {"type": "boolean", "description": "list: also resolved comments"}}), &["action"]),
        tool("audition", "A/B listening tests for the human. create renders each variant (a list of edit commands applied to a copy of the \
current project; the current project itself is variant A unless include_current=false), cuts the bars from_bar..to_bar, matches their \
loudness and shows them in the GUI's Auditions tab, where the human plays them and picks one. wait blocks until they pick; apply \
makes the chosen variant's edits for real (one undo step).", json!({
            "action": {"enum": ["create", "list", "wait", "apply", "play"]},
            "id": {"type": "integer", "description": "audition id (default: the newest)"},
            "label": {"type": "string"}, "question": {"type": "string", "description": "what the human should listen for"},
            "from_bar": {"type": "integer", "minimum": 1}, "to_bar": {"type": "integer", "minimum": 1},
            "variants": {"type": "array", "minItems": 1, "maxItems": 6, "items": obj(json!({"name": {"type": "string"},
                "commands": {"type": "array", "items": command_s()}}), &["name", "commands"])},
            "include_current": {"type": "boolean", "description": "default true"},
            "variant": {"description": "apply/play: letter (A, B...), name or 0-based index (default: the human's pick)"},
            "timeout_s": {"type": "number", "description": "wait: default 600"}}), &["action"]),
        tool("activity", "The activity log: who changed what (source agent / human / system), newest last.", json!({
            "since_revision": {"type": "integer"}, "limit": {"type": "integer"}, "source": {"enum": ["agent", "human", "system", "any"]}}), &[]),
        tool("gui", "The visual app: open the window, focus a track/bar/panel (so the human sees what you are working on), or take a screenshot.", json!({
            "action": {"enum": ["open", "focus", "screenshot"]}, "track": track_ref(), "bar": {"type": "integer", "minimum": 1},
            "panel": {"enum": PANELS, "description": "focus: switch the main view (arrange, piano_roll) or the bottom tab (mixer, track, master, analysis, comments, auditions)"},
            "max_width": {"type": "integer", "minimum": 200, "description": "screenshot: downscale to at most this many pixels wide (default 1600)"}}), &["action"]),
        crate::plugins::tool(),
        flux_score::compose_tool(),
        flux_score::lint_tool(),
    ]
    .into_iter()
    .chain(crate::measure::list())
    .collect()
}

// ---------------------------------------------------------------------------------------------
// dispatch

pub fn call(shared: &Arc<Shared>, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = params.get("arguments").cloned().filter(|a| a.is_object()).unwrap_or(json!({}));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(shared, name, &args)));
    let (content, is_error) = match r {
        Ok(Ok(c)) => (c, false),
        Ok(Err(e)) => (vec![Content::Text(format!("error: {e:#}"))], true),
        Err(p) => {
            let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            (vec![Content::Text(format!("internal error: {msg}"))], true)
        }
    };
    let content: Vec<Value> = content
        .into_iter()
        .map(|c| match c {
            Content::Text(t) => json!({"type": "text", "text": t}),
            Content::Png(b) => {
                use base64::Engine;
                json!({"type": "image", "mimeType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(b)})
            }
        })
        .collect();
    json!({"content": content, "isError": is_error})
}

fn dispatch(sh: &Arc<Shared>, name: &str, a: &Value) -> Result<Vec<Content>> {
    match name {
        "get_project" => get_project(sh, a),
        "get_notes" => get_notes(sh, a),
        "edit" => edit(sh, a),
        "undo" => undo_redo(sh, a, true),
        "redo" => undo_redo(sh, a, false),
        "new_project" => new_project(sh, a),
        "open" => open(sh, a),
        "save" => save(sh, a),
        "list_instruments" => list_instruments(a),
        "probe_instrument" => probe_instrument(a),
        "sfz_sets" => sfz_sets(a),
        "sfz_derive" => sfz_derive(a),
        "render" => render(sh, a),
        "analyze" => analyze(sh, a),
        "export" => export(sh, a),
        "transport" => transport(sh, a),
        "comments" => comments(sh, a),
        "audition" => audition(sh, a),
        "activity" => activity(sh, a),
        "gui" => gui(sh, a),
        "plugins" => crate::plugins::call(sh, a),
        "compose" => compose(sh, a),
        "lint" => lint(sh, a),
        _ => crate::measure::call(sh, name, a),
    }
}

pub(crate) fn get<T: DeserializeOwned>(a: &Value, k: &str) -> Result<Option<T>> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone()).map(Some).with_context(|| format!("argument '{k}'")),
    }
}

pub(crate) fn req<T: DeserializeOwned>(a: &Value, k: &str) -> Result<T> {
    get(a, k)?.ok_or_else(|| anyhow!("missing argument '{k}'"))
}

pub(crate) fn text(v: Value) -> Result<Vec<Content>> {
    Ok(vec![Content::Text(serde_json::to_string(&v)?)])
}

fn r1(x: f64) -> Value {
    if x.is_finite() { json!((x * 10.0).round() / 10.0) } else { Value::Null }
}

fn num(x: f64) -> String {
    let s = format!("{:.3}", x + 0.0);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

pub(crate) fn rel(p: &Path) -> String {
    paths::portable(p)
}

/// Where a project is saved when it has no path yet.
pub fn default_project_path(name: &str) -> PathBuf {
    paths::projects_dir().join(format!("{}.flux.json", slug(name)))
}

fn slug(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "untitled".into() } else { s }
}

// ---------------------------------------------------------------------------------------------
// reading

pub fn render_status(st: &State) -> Value {
    let rendered = st.render.as_ref().map(|r| r.revision);
    let status = match &st.status {
        RenderStatus::Idle => "idle".to_string(),
        RenderStatus::Running { revision, started, message } => {
            format!("rendering revision {revision}: {message} ({:.0} s)", started.elapsed().as_secs_f64())
        }
        RenderStatus::Failed { revision, error } => format!("render of revision {revision} failed: {error}"),
    };
    json!({"status": status, "rendered_revision": rendered, "stale": rendered != Some(st.session.revision)})
}

fn mixer_diff(m: &Mixer) -> Value {
    let mut o = Map::new();
    if let (Ok(Value::Object(a)), Ok(Value::Object(d))) = (serde_json::to_value(m), serde_json::to_value(Mixer::default())) {
        for (k, v) in a {
            if d.get(&k) != Some(&v) {
                o.insert(k, v);
            }
        }
    }
    Value::Object(o)
}

/// An insert chain, compactly: id, plugin, preset name, and what differs from the usual.
fn inserts_json(chain: &[Insert]) -> Value {
    json!(chain
        .iter()
        .map(|i| {
            let mut o = json!({"id": i.id, "plugin": i.plugin_name()});
            if !i.preset_name.is_empty() {
                o["preset_name"] = json!(i.preset_name);
            } else if i.preset.is_some() {
                o["preset_name"] = json!("(captured)");
            }
            if !i.params.is_empty() {
                o["params"] = json!(i.params);
            }
            if i.bypass {
                o["bypass"] = json!(true);
            }
            if i.position == InsertPosition::Pre {
                o["position"] = json!("pre");
            }
            o
        })
        .collect::<Vec<_>>())
}

fn instrument_json(i: &Instrument) -> Value {
    match i {
        Instrument::Sfz { path, init_cc } => json!({"kind": "sfz", "path": path, "init_cc": init_cc}),
        Instrument::Vst3 { path, params, preset, preset_name, class } => json!({"kind": "vst3", "path": path, "params": params, "preset": preset, "preset_name": preset_name, "class": class}),
        Instrument::Audio { .. } => json!({"kind": "audio"}),
        Instrument::None => json!({"kind": "none"}),
    }
}

/// get_project lists this many of a track's clips.
const MAX_CLIPS_LISTED: usize = 24;

/// A clip in a line: id, file, where it plays (bar:beat to bar:beat), and what differs from the whole file as is.
fn clip_text(tm: &TimeMap, c: &clip::Clip) -> String {
    let at = |beat: f64| {
        let bar = tm.bar_of(beat + clip::EPS);
        format!("{bar}:{}", num(beat - tm.bar_beat(bar)))
    };
    let file = flux_engine::clips::info(&paths::resolve(&c.path));
    let mut s = format!("#{} {} at {}", c.id, c.path, at(c.start));
    if let Some(e) = c.end(tm, file.as_ref().ok().map(|i| i.seconds())) {
        s += &format!(" to {}", at(e));
    }
    if c.offset != 0.0 {
        s += &format!(", from {} s into the file", num(c.offset));
    }
    if let Some(l) = c.length {
        s += &format!(", {} s long", num(l));
    }
    if c.gain_db != 0.0 {
        s += &format!(", {} dB", num(c.gain_db));
    }
    if c.fade_in != 0.0 || c.fade_out != 0.0 {
        s += &format!(", fades {}/{} s", num(c.fade_in), num(c.fade_out));
    }
    if let Err(e) = file {
        s += &format!(" (can't read the file: {e})");
    }
    s
}

/// Automation lanes in a line each: what moves, how many points, over which bars.
fn lanes_json(tm: &TimeMap, lanes: &[Lane]) -> Value {
    json!(lanes
        .iter()
        .filter_map(|l| {
            let (first, last) = (l.points.first()?, l.points.last()?);
            let norm = if l.normalized { ", normalized" } else { "" };
            Some(format!("{}: {} points{norm}, bars {}-{}", l.target.label(), l.points.len(), tm.bar_of(first.beat + 1e-9), tm.bar_of(last.beat + 1e-9)))
        })
        .collect::<Vec<_>>())
}

/// A track's name for routing displays (its id if it's gone).
fn track_name(p: &Project, id: u64) -> Value {
    p.tracks.iter().find(|t| t.id == id).map_or_else(|| json!(id), |t| json!(t.name))
}

/// What feeds a bus: the tracks whose output goes to it and the sends to it, with their levels. None if nothing does.
fn bus_json(p: &Project, id: u64) -> Option<Value> {
    let routed: Vec<&str> = p.tracks.iter().filter(|t| t.mixer.output == Some(id)).map(|t| t.name.as_str()).collect();
    let sends: Vec<Value> = p
        .tracks
        .iter()
        .flat_map(|t| t.mixer.sends.iter().filter(|s| s.to == id).map(|s| json!({"from": t.name, "level": s.level})))
        .collect();
    if routed.is_empty() && sends.is_empty() {
        return None;
    }
    let mut o = Map::new();
    if !routed.is_empty() {
        o.insert("routed".into(), json!(routed));
    }
    if !sends.is_empty() {
        o.insert("sends".into(), json!(sends));
    }
    Some(Value::Object(o))
}

fn track_summary(p: &Project, tm: &TimeMap, t: &Track) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), json!(t.id));
    o.insert("name".into(), json!(t.name));
    o.insert("instrument".into(), instrument_json(&t.instrument));
    if let Some(b) = bus_json(p, t.id) {
        o.insert("bus".into(), b);
    }
    if !t.layer.is_empty() {
        o.insert("layer".into(), json!(t.layer));
    }
    o.insert("notes".into(), json!(t.notes.len()));
    if !t.notes.is_empty() {
        let lo = t.notes.iter().map(|n| n.pitch).min().unwrap();
        let hi = t.notes.iter().map(|n| n.pitch).max().unwrap();
        let vlo = t.notes.iter().map(|n| n.vel).min().unwrap();
        let vhi = t.notes.iter().map(|n| n.vel).max().unwrap();
        let first = t.notes.iter().map(|n| n.start).fold(f64::MAX, f64::min);
        let last = t.notes.iter().map(|n| n.start + n.dur).fold(0.0, f64::max);
        o.insert("range".into(), json!(format!("{}-{}", pitch::name(lo), pitch::name(hi))));
        o.insert("velocity".into(), json!(format!("{vlo}-{vhi}")));
        o.insert("bars".into(), json!(format!("{}-{}", tm.bar_of(first), tm.bar_of((last - 1e-6).max(first)))));
    }
    if !t.ccs.is_empty() {
        let mut ccs: Vec<u8> = t.ccs.iter().map(|c| c.cc).collect();
        ccs.sort();
        ccs.dedup();
        o.insert("cc".into(), json!(ccs));
    }
    if !t.bends.is_empty() {
        o.insert("bends".into(), json!(t.bends.len()));
    }
    let mut m = mixer_diff(&t.mixer);
    if let Some(m) = m.as_object_mut() {
        m.remove("inserts");
        if let Some(id) = t.mixer.output {
            m.insert("output".into(), track_name(p, id));
        }
        if !t.mixer.sends.is_empty() {
            m.insert("sends".into(), json!(t.mixer.sends.iter().map(|s| json!({"to": track_name(p, s.to), "level": s.level})).collect::<Vec<_>>()));
        }
    }
    if m.as_object().is_some_and(|m| !m.is_empty()) {
        o.insert("mixer".into(), m);
    }
    if !t.mixer.inserts.is_empty() {
        o.insert("inserts".into(), inserts_json(&t.mixer.inserts));
    }
    if !t.automation.is_empty() {
        o.insert("automation".into(), lanes_json(tm, &t.automation));
    }
    if !t.clips.is_empty() {
        let mut v: Vec<Value> = t.clips.iter().take(MAX_CLIPS_LISTED).map(|c| json!(clip_text(tm, c))).collect();
        if t.clips.len() > MAX_CLIPS_LISTED {
            v.push(json!(format!("... {} more (get_notes lists them by bar)", t.clips.len() - MAX_CLIPS_LISTED)));
        }
        o.insert("clips".into(), json!(v));
    }
    if let Some(s) = t.meta.get("sfz")
        && !matches!(t.instrument, Instrument::Sfz { .. }) {
            o.insert("sfz_hint".into(), json!(rel(Path::new(s))));
        }
    Value::Object(o)
}

fn get_project(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let st = sh.lock();
    let p = &st.session.project;
    if get::<String>(a, "detail")?.as_deref() == Some("full") {
        return text(json!({"revision": st.session.revision, "project": p}));
    }
    let tm = TimeMap::new(p);
    let mut o = Map::new();
    o.insert("revision".into(), json!(st.session.revision));
    o.insert("path".into(), json!(st.session.path.as_ref().map(|p| rel(p))));
    o.insert("name".into(), json!(p.name));
    if !p.key.is_empty() {
        o.insert("key".into(), json!(p.key));
    }
    o.insert("time_signature".into(), json!(p.time_signature));
    o.insert("tempo".into(), json!(p.tempo));
    o.insert("bars".into(), json!(p.bars));
    o.insert("looped".into(), json!(p.looped));
    o.insert("duration_s".into(), r1(tm.sec(p.total_beats())));
    o.insert("sections".into(), json!(p.sections.iter().map(|s| json!([s.name, format!("{}-{}", s.start_bar, s.start_bar + s.bars - 1)])).collect::<Vec<_>>()));
    if !p.harmony.is_empty() {
        o.insert("harmony".into(), json!(p.harmony.iter().map(|c| if c.beat == 0.0 { format!("{}:{}", c.bar, c.chord) } else { format!("{}@{}:{}", c.bar, c.beat, c.chord) }).collect::<Vec<_>>()));
    }
    o.insert("tracks".into(), json!(p.tracks.iter().map(|t| track_summary(p, &tm, t)).collect::<Vec<_>>()));
    let mut master = json!(p.master);
    if let Some(m) = master.as_object_mut() {
        for (k, chain) in [("inserts", &p.master.inserts), ("reverb_inserts", &p.master.reverb_inserts)] {
            if !chain.is_empty() {
                m.insert(k.into(), inserts_json(chain));
            }
        }
        if p.master.automation.is_empty() {
            m.remove("automation");
        } else {
            m.insert("automation".into(), lanes_json(&tm, &p.master.automation));
        }
    }
    o.insert("master".into(), master);
    if !p.notes.is_empty() {
        o.insert("notes".into(), json!(p.notes));
    }
    let open: Vec<Value> = p.comments.iter().filter(|c| !c.resolved).map(|c| comment_json(p, c)).collect();
    if !open.is_empty() {
        o.insert("open_comments".into(), json!(open));
    }
    let waiting: Vec<Value> = st.auditions.iter().filter(|a| !a.applied).map(audition_json).collect();
    if !waiting.is_empty() {
        o.insert("auditions".into(), json!(waiting));
    }
    o.insert("render".into(), render_status(&st));
    o.insert("playback".into(), json!({"mode": mode_name(sh.player.mode()), "playing": sh.player.is_playing()}));
    o.insert("undo".into(), json!({"can_undo": st.session.can_undo(), "can_redo": st.session.can_redo()}));
    o.insert("human_watching".into(), json!(st.gui_open));
    text(Value::Object(o))
}

fn get_notes(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let tr: TrackRef = req(a, "track")?;
    let st = sh.lock();
    let p = &st.session.project;
    let t = &p.tracks[p.track_index(&tr).ok_or_else(|| anyhow!("no track {tr}"))?];
    let tm = TimeMap::new(p);
    let from = get::<u32>(a, "from_bar")?.unwrap_or(1).max(1);
    let to = get::<u32>(a, "to_bar")?.unwrap_or(p.bars).max(from);
    let with_cc = get::<bool>(a, "include_cc")?.unwrap_or(true);
    let mut out = format!(
        "track '{}' (#{}), bars {from}-{to}; each note: beat pitch dur vel (beat = offset in the bar, durations in beats)\n",
        t.name, t.id
    );
    let mut empty = 0;
    for bar in from..=to {
        let b0 = tm.bar_beat(bar);
        let b1 = tm.bar_beat(bar + 1);
        let notes: Vec<String> = t
            .notes
            .iter()
            .filter(|n| n.start >= b0 - 1e-9 && n.start < b1 - 1e-9)
            .map(|n| format!("{} {} {} {}", num(n.start - b0), pitch::name(n.pitch), num(n.dur), n.vel))
            .collect();
        if notes.is_empty() {
            empty += 1;
            continue;
        }
        let sec = p.section_at(bar).map(|s| format!(" [{}]", s.name)).unwrap_or_default();
        out.push_str(&format!("bar {bar}{sec}: {}\n", notes.join(" | ")));
    }
    if empty > 0 {
        out.push_str(&format!("({empty} bars without notes)\n"));
    }
    if with_cc && !t.ccs.is_empty() {
        let (a0, a1) = (tm.bar_beat(from), tm.bar_beat(to + 1));
        let mut ccs: Vec<u8> = t.ccs.iter().map(|c| c.cc).collect();
        ccs.sort();
        ccs.dedup();
        for cc in ccs {
            let pts: Vec<_> = t.ccs.iter().filter(|c| c.cc == cc && c.beat >= a0 - 1e-9 && c.beat < a1 - 1e-9).collect();
            if pts.is_empty() {
                continue;
            }
            if pts.len() <= 48 {
                let s: Vec<String> = pts
                    .iter()
                    .map(|c| {
                        let bar = tm.bar_of(c.beat + 1e-9);
                        format!("{bar}:{}={}", num(c.beat - tm.bar_beat(bar)), c.value)
                    })
                    .collect();
                out.push_str(&format!("cc{cc} (bar:beat=value): {}\n", s.join(" ")));
            } else {
                let mut per_bar = vec![];
                for bar in from..=to {
                    let v: Vec<u8> = pts.iter().filter(|c| tm.bar_of(c.beat + 1e-9) == bar).map(|c| c.value).collect();
                    if let (Some(lo), Some(hi)) = (v.iter().min(), v.iter().max()) {
                        per_bar.push(if lo == hi { format!("{bar}:{lo}") } else { format!("{bar}:{lo}-{hi}") });
                    }
                }
                out.push_str(&format!("cc{cc} ({} points; per-bar range): {}\n", pts.len(), per_bar.join(" ")));
            }
        }
    }
    if !t.bends.is_empty() {
        out.push_str(&format!("{} pitch-bend points\n", t.bends.len()));
    }
    let (a0, a1) = (tm.bar_beat(from), tm.bar_beat(to + 1));
    for l in &t.automation {
        out.push_str(&lane_text(&tm, l, a0, a1, from, to));
    }
    // the clips playing in these bars
    let clips: Vec<String> = t
        .clips
        .iter()
        .filter(|c| {
            let len = flux_engine::clips::info(&paths::resolve(&c.path)).ok().map(|i| i.seconds());
            c.start < a1 - clip::EPS && c.end(&tm, len).is_none_or(|e| e > a0 + clip::EPS)
        })
        .map(|c| clip_text(&tm, c))
        .collect();
    if !clips.is_empty() {
        out.push_str(&format!("audio clips (bar:beat; offset, length and fades in seconds):\n  {}\n", clips.join("\n  ")));
    } else if !t.clips.is_empty() {
        out.push_str(&format!("no audio clips in these bars ({} elsewhere)\n", t.clips.len()));
    }
    Ok(vec![Content::Text(out)])
}

/// A lane's points in beats `[a0, a1)` (bars `from`-`to`) as `bar:beat=value`, a shape after one that isn't linear;
/// a long lane as each bar's range of values.
fn lane_text(tm: &TimeMap, l: &Lane, a0: f64, a1: f64, from: u32, to: u32) -> String {
    let pts: Vec<_> = l.points.iter().filter(|q| q.beat >= a0 - 1e-9 && q.beat < a1 - 1e-9).collect();
    let unit = match (&l.target, l.normalized) {
        (_, true) => ", normalized",
        (flux_core::automation::Target::Volume, _) => ", dB",
        _ => "",
    };
    let label = l.target.label();
    if pts.is_empty() {
        return format!("automation {label}: no points in these bars ({} elsewhere)\n", l.points.len());
    }
    if pts.len() <= 48 {
        let s: Vec<String> = pts
            .iter()
            .map(|q| {
                let bar = tm.bar_of(q.beat + 1e-9);
                let shape = match q.shape {
                    Shape::Linear => "",
                    Shape::Smooth => " smooth",
                    Shape::Hold => " hold",
                };
                format!("{bar}:{}={}{shape}", num(q.beat - tm.bar_beat(bar)), num(q.value))
            })
            .collect();
        return format!("automation {label} (bar:beat=value{unit}): {}\n", s.join(" "));
    }
    let mut per_bar = vec![];
    for bar in from..=to {
        let v: Vec<f64> = pts.iter().filter(|q| tm.bar_of(q.beat + 1e-9) == bar).map(|q| q.value).collect();
        let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if !v.is_empty() {
            per_bar.push(if lo == hi { format!("{bar}:{}", num(lo)) } else { format!("{bar}:{}..{}", num(lo), num(hi)) });
        }
    }
    format!("automation {label} ({} points{unit}; per-bar range): {}\n", pts.len(), per_bar.join(" "))
}

// ---------------------------------------------------------------------------------------------
// editing

fn edit(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let raw: Vec<Value> = req(a, "commands")?;
    let mut cmds = Vec::with_capacity(raw.len());
    for (i, c) in raw.into_iter().enumerate() {
        let cmd: Command = serde_json::from_value(c).with_context(|| format!("command {}", i + 1))?;
        cmds.push(cmd);
    }
    if cmds.is_empty() {
        bail!("no commands");
    }
    let results = sh.apply(&cmds, Source::Agent)?;
    let rev = sh.lock().session.revision;
    text(json!({"ok": true, "revision": rev, "results": results}))
}

/// The composing toolkit: the commands it returns go in as one undo step, under the action's label, with
/// the project locked from reading it to applying them (an edit can't slip in between).
fn compose(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (out, rev) = sh.with_session(|s| -> Result<_> {
        let out = flux_score::compose(&s.project, a)?;
        if !out.commands.is_empty() {
            s.apply_batch_labeled(&out.commands, Source::Agent, Some(&out.label))?;
        }
        Ok((out, s.revision))
    })?;
    let mut r = json!({"ok": true, "action": out.action, "summary": out.summary, "commands": out.commands.len(), "revision": rev});
    if !out.warnings.is_empty() {
        r["warnings"] = json!(out.warnings);
    }
    text(r)
}

/// The score linter, with the instrument cards in catalog/ (loaded once) for the range and keyswitch checks.
fn lint(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    static BASE: std::sync::OnceLock<flux_score::LintOptions> = std::sync::OnceLock::new();
    let base = BASE.get_or_init(|| {
        let catalog = flux_score::InstrumentCatalog::load_dir(&paths::root().join("catalog")).ok().map(|(c, skipped)| {
            for s in skipped {
                eprintln!("flux: catalog card skipped: {s}");
            }
            c
        });
        flux_score::LintOptions { catalog, ..Default::default() }
    });
    let project = sh.lock().session.project.clone();
    let v = flux_score::lint::lint_handler_with(&project, a, base)?;
    match v.get("text").and_then(Value::as_str) {
        Some(t) => Ok(vec![Content::Text(t.to_string())]),
        None => text(v),
    }
}

fn undo_redo(sh: &Arc<Shared>, a: &Value, undo: bool) -> Result<Vec<Content>> {
    let steps = get::<u32>(a, "steps")?.unwrap_or(1).max(1);
    let done = sh.with_session(|s| (0..steps).take_while(|_| if undo { s.undo(Source::Agent) } else { s.redo(Source::Agent) }).count());
    let st = sh.lock();
    text(json!({"steps_done": done, "revision": st.session.revision, "can_undo": st.session.can_undo(), "can_redo": st.session.can_redo()}))
}

fn new_project(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let name: String = req(a, "name")?;
    let mut p = Project::new(&name, req(a, "bpm")?, req(a, "bars")?);
    if let Some(ts) = get::<(u32, u32)>(a, "time_signature")? {
        p.time_signature = ts;
    }
    if let Some(l) = get::<bool>(a, "looped")? {
        p.looped = l;
    }
    if let Some(k) = get::<String>(a, "key")? {
        p.key = k;
    }
    sh.with_session(|s| {
        s.replace(p, Source::Agent, &format!("new project '{name}'"));
        s.path = None;
    });
    text(json!({"ok": true, "revision": sh.lock().session.revision}))
}

/// Load any supported file into a project. Returns (project, is a Flux project file).
pub fn load_any(path: &Path) -> Result<(Project, bool)> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext == "mid" || ext == "midi" {
        return Ok((import::import_midi(path)?, false));
    }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)
        .with_context(|| format!("parsing {}", path.display()))?;
    if v.get("tracks").is_some() && v.get("master").is_some() {
        Ok((serde_json::from_value(v).context("not a valid Flux project")?, true))
    } else if v.get("title").is_some() {
        Ok((import::import_pipeline(path)?, false))
    } else {
        bail!("{} is neither a Flux project nor a pipeline song description", path.display())
    }
}

/// Open a project, pipeline song or MIDI file into the session (also used by the GUI's Open and drag-and-drop).
pub fn open_file(sh: &Arc<Shared>, path: &Path, source: Source) -> Result<Value> {
    let (p, native) = load_any(path)?;
    let (name, tracks, notes) = (p.name.clone(), p.tracks.len(), p.notes.clone());
    sh.with_session(|s| {
        s.replace(p, source, &format!("opened {}", rel(path)));
        s.path = native.then(|| path.to_path_buf());
    });
    if sh.player.mode() == crate::audio::Mode::Live {
        sh.player.stop();
        sh.player.seek(0.0);
    }
    let mut o = json!({"ok": true, "name": name, "tracks": tracks, "revision": sh.lock().session.revision});
    if !native {
        o["note"] = json!("imported: save to keep it as a Flux project");
    }
    if !notes.is_empty() {
        o["import_notes"] = json!(notes);
    }
    Ok(o)
}

/// Project file names: Flux's, and Baton's from before the rename (still read).
pub fn is_project_file(p: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    name.ends_with(".flux.json") || name.ends_with(".baton.json")
}

/// Flux projects in the projects folder, newest first.
pub fn project_files() -> Vec<PathBuf> {
    let dir = paths::projects_dir();
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| is_project_file(p))
                .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    v.into_iter().map(|x| x.1).collect()
}

fn open(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let path = paths::resolve(&req::<String>(a, "path")?);
    text(open_file(sh, &path, Source::Agent)?)
}

fn save(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let given = get::<String>(a, "path")?.map(|p| paths::resolve(&p));
    let saved = sh.with_session(|s| {
        let path = given.or_else(|| s.path.clone()).unwrap_or_else(|| default_project_path(&s.project.name));
        s.save(Some(&path))
    })?;
    text(json!({"saved": rel(&saved)}))
}

// ---------------------------------------------------------------------------------------------
// instruments

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, want: &dyn Fn(&Path) -> Option<bool>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match want(&p) {
            Some(true) => out.push(p),
            Some(false) => {}
            None => {
                if depth > 0 && p.is_dir() {
                    walk(&p, depth - 1, out, want);
                }
            }
        }
    }
}

fn is_sfz(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sfz"))
}

fn is_rig(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().ends_with(".rig.json"))
}

/// Seconds, to the millisecond.
fn secs(t: Instant) -> Value {
    json!((t.elapsed().as_secs_f64() * 1000.0).round() / 1000.0)
}

fn list_instruments(a: &Value) -> Result<Vec<Content>> {
    let q = get::<String>(a, "query")?.unwrap_or_default().to_lowercase();
    let kind = get::<String>(a, "kind")?.unwrap_or("any".into());
    if !["sfz", "vst3", "rig", "any"].contains(&kind.as_str()) {
        bail!("kind must be sfz, vst3, rig or any (not '{kind}')");
    }
    let limit = get::<usize>(a, "limit")?.unwrap_or(80);
    let programs_only = get::<bool>(a, "programs_only")?.unwrap_or(true);
    let details = get::<bool>(a, "details")?.unwrap_or(false);
    let t0 = Instant::now();
    let root = paths::root();
    let mut found = vec![];
    let wanted = |k: &str| kind == "any" || kind == k;
    if wanted("sfz") {
        let sfz = |p: &Path| -> Option<bool> {
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sfz")) {
                Some(true)
            } else if p.is_dir() {
                None
            } else {
                Some(false)
            }
        };
        for d in ["build/sfz", "sfz", "instruments", "daw/instruments"] {
            walk(&root.join(d), 7, &mut found, &sfz);
        }
    }
    if wanted("rig") {
        let rig = |p: &Path| -> Option<bool> {
            if is_rig(p) {
                Some(true)
            } else if p.is_dir() {
                None
            } else {
                Some(false)
            }
        };
        walk(&root.join("sfz").join("rigs"), 4, &mut found, &rig);
    }
    if wanted("vst3") {
        let vst = |p: &Path| -> Option<bool> {
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3")) {
                Some(true)
            } else if p.is_dir() {
                None
            } else {
                Some(false)
            }
        };
        let mut dirs = vec![root.join("plugins"), root.join("daw").join("plugins")];
        if let Ok(cf) = std::env::var("CommonProgramFiles") {
            dirs.push(PathBuf::from(cf).join("VST3"));
        }
        for d in dirs {
            walk(&d, 4, &mut found, &vst);
        }
    }
    // fragments are found among every SFZ file (the program including one may not match the query); the
    // classification is cached, so only files changed since the last call are read again
    let sfz_files: Vec<PathBuf> = found.iter().filter(|p| is_sfz(p)).cloned().collect();
    let classes = (programs_only && !sfz_files.is_empty()).then(|| flux_sfz::catalog::classify(root, &sfz_files));
    let fragment = |p: &Path| classes.as_ref().is_some_and(|c| is_sfz(p) && c.is_fragment(p));
    let matching: Vec<&PathBuf> = found.iter().filter(|p| q.is_empty() || rel(p).to_lowercase().contains(&q)).collect();
    let hidden = matching.iter().filter(|p| fragment(p)).count();
    let all: Vec<&PathBuf> = matching.into_iter().filter(|p| !fragment(p)).collect();
    let total = all.len();
    let instruments: Vec<Value> = all
        .iter()
        .take(limit)
        .map(|p| {
            if !details {
                return json!(rel(p));
            }
            if is_rig(p) {
                return match flux_engine::rig::describe(p) {
                    Ok(Value::Object(mut m)) => {
                        m.insert("path".into(), json!(rel(p)));
                        Value::Object(m)
                    }
                    Ok(_) => json!({"path": rel(p)}),
                    Err(e) => json!({"path": rel(p), "error": format!("{e:#}")}),
                };
            }
            if !is_sfz(p) {
                return json!({"path": rel(p)});
            }
            match flux_sfz::catalog::summary(p).map(serde_json::to_value) {
                Ok(Ok(Value::Object(mut m))) => {
                    m.insert("path".into(), json!(rel(p)));
                    Value::Object(m)
                }
                Ok(Ok(_)) => json!({"path": rel(p)}),
                Ok(Err(e)) => json!({"path": rel(p), "error": e.to_string()}),
                Err(e) => json!({"path": rel(p), "error": e.to_string()}),
            }
        })
        .collect();
    let mut out = json!({"total": total, "shown": instruments.len(), "instruments": instruments, "seconds": secs(t0),
        "hint": "build/sfz/<set>/ holds the patched wrappers the songs use (dynamics on CC1, keyswitches at 120+; see sfz_sets)"});
    // libraries Flux supports that this computer lacks (plugins scan lists the plugins)
    if kind != "vst3" {
        let missing: Vec<Value> = crate::supported::missing(&crate::supported::packs(), flux_assets::packs::Section::Instruments, &q).into_iter().take(limit).collect();
        if !missing.is_empty() {
            out["not_installed"] = json!(missing);
        }
    }
    if classes.is_some() {
        out["fragments_hidden"] = json!(hidden);
    }
    if let Some(c) = classes.as_ref().filter(|c| !c.unreadable.is_empty()) {
        out["unreadable"] = json!(c.unreadable.iter().take(5).map(|(p, e)| format!("{}: {e}", rel(p))).collect::<Vec<_>>());
    }
    text(out)
}

fn probe_instrument(a: &Value) -> Result<Vec<Content>> {
    let path = paths::resolve(&req::<String>(a, "path")?);
    if !is_sfz(&path) {
        bail!("probe_instrument supports SFZ files");
    }
    let render = get::<bool>(a, "render")?.unwrap_or(true);
    let format = get::<String>(a, "format")?.unwrap_or_else(|| "text".into());
    if !matches!(format.as_str(), "text" | "json") {
        bail!("format must be \"text\" or \"json\"");
    }
    let t0 = Instant::now();
    let inst = flux_sfz::Instrument::load(&path)?;
    let info = flux_sfz::info::describe(&inst);
    let mut out = Map::new();
    out.insert("path".into(), json!(rel(&path)));
    if format == "json" {
        out.insert("info".into(), serde_json::to_value(&info)?);
    } else {
        out.insert("description".into(), json!(info.to_text()));
        out.insert("keyswitches".into(), serde_json::to_value(&info.ranges.keyswitches)?);
        out.insert("controllers".into(), serde_json::to_value(&info.ranges.ccs)?);
    }
    if render {
        out.extend(probe_render(&path, a)?);
    } else {
        out.insert("regions".into(), json!(info.ranges.playable_regions));
        out.insert("seconds".into(), secs(t0));
    }
    text(Value::Object(out))
}

/// Play every key of `path` through sfizz: which keys sound and how loud, by octave.
fn probe_render(path: &Path, a: &Value) -> Result<Map<String, Value>> {
    let vel = get::<u8>(a, "velocity")?.unwrap_or(100).clamp(1, 127);
    let from = get::<flux_core::pitch::PitchSpec>(a, "from")?.map(|p| p.resolve()).transpose()?.unwrap_or(0);
    let to = get::<flux_core::pitch::PitchSpec>(a, "to")?.map(|p| p.resolve()).transpose()?.unwrap_or(127).max(from);
    let ccs: std::collections::BTreeMap<String, u8> = get(a, "cc")?.unwrap_or_default();
    let sr = 44100u32;
    let t0 = Instant::now();
    let mut synth = sfizz::Synth::new(path, sr, 256)?;
    let spacing = (0.45 * sr as f64) as u64;
    let hold = (0.25 * sr as f64) as u64;
    let lead = (0.1 * sr as f64) as u64;
    let mut ev: Vec<(u64, sfizz::Ev)> = vec![];
    for (k, v) in &ccs {
        ev.push((0, sfizz::Ev::Cc(k.parse().context("cc numbers")?, *v)));
    }
    for (i, key) in (from..=to).enumerate() {
        let t = lead + i as u64 * spacing;
        ev.push((t, sfizz::Ev::On(key, vel)));
        ev.push((t + hold, sfizz::Ev::Off(key)));
    }
    ev.sort_by_key(|e| e.0);
    let frames = (lead + (to - from + 1) as u64 * spacing + sr as u64) as usize;
    let (l, r) = synth.render(&ev, frames);
    let win = |a: usize, b: usize| -> (f64, f64) {
        let (mut pk, mut s) = (0f64, 0f64);
        for k in a..b.min(l.len()) {
            let m = l[k].abs().max(r[k].abs()) as f64;
            pk = pk.max(m);
            s += m * m;
        }
        (20.0 * pk.max(1e-12).log10(), 10.0 * (s / (b.saturating_sub(a)).max(1) as f64 + 1e-24).log10())
    };
    let mut sounding = vec![];
    let mut levels = std::collections::BTreeMap::<i32, Vec<f64>>::new();
    for (i, key) in (from..=to).enumerate() {
        let t = (lead + i as u64 * spacing) as usize;
        let (_, before) = win(t.saturating_sub(2205), t);
        let (pk, after) = win(t, t + 6615);
        let on = pk > -60.0 && after > before + 6.0;
        if on {
            levels.entry(key as i32 / 12 - 1).or_default().push(pk);
        }
        sounding.push((key, on));
    }
    let mut ranges = vec![];
    let mut i = 0;
    while i < sounding.len() {
        if sounding[i].1 {
            let s = sounding[i].0;
            let mut j = i;
            while j + 1 < sounding.len() && sounding[j + 1].1 {
                j += 1;
            }
            let e = sounding[j].0;
            ranges.push(if s == e { pitch::name(s) } else { format!("{}-{} ({}-{})", pitch::name(s), pitch::name(e), s, e) });
            i = j + 1;
        } else {
            i += 1;
        }
    }
    let by_oct: Map<String, Value> = levels
        .iter()
        .map(|(o, v)| (format!("octave {o}"), r1(v.iter().sum::<f64>() / v.len() as f64)))
        .collect();
    match json!({
        "regions": synth.num_regions, "velocity": vel,
        "sounding_ranges": ranges, "sounding_keys": sounding.iter().filter(|s| s.1).count(),
        "mean_peak_dbfs_by_octave": by_oct, "probe_seconds": r1(t0.elapsed().as_secs_f64()),
        "note": "keys that only switch articulations (keyswitches) or need a CC (e.g. dynamics on CC1) can read as silent; pass cc to test",
    }) {
        Value::Object(m) => Ok(m),
        _ => unreachable!("a JSON object literal"),
    }
}

/// Instrument sets: check, build or list untracked files (see `flux sfz help`).
fn sfz_sets(a: &Value) -> Result<Vec<Content>> {
    use flux_sfz::manifest::{self, BuildOptions, CheckOptions};
    let action = req::<String>(a, "action")?;
    let names = get::<Vec<String>>(a, "sets")?.unwrap_or_default();
    let root = paths::root();
    let sets = flux_sfz::set::SetFile::load_named(&root.join("sfz").join("sets"), root, &names)?;
    let t0 = Instant::now();
    let mut out = match action.as_str() {
        "check" => {
            let options = CheckOptions { regenerate: get::<bool>(a, "regenerate")?.unwrap_or(false) };
            let checks = sets.iter().map(|s| manifest::check(s, options)).collect::<flux_sfz::Result<Vec<_>>>()?;
            json!({"clean": checks.iter().all(|c| c.is_clean()), "sets": checks})
        }
        "build" => {
            let options = BuildOptions {
                force: get::<bool>(a, "force")?.unwrap_or(false),
                dry_run: get::<bool>(a, "dry_run")?.unwrap_or(false),
                only: get::<Vec<String>>(a, "only")?.unwrap_or_default(),
            };
            let reports = manifest::build_sets(&sets, &options)?;
            let failed = reports.iter().map(|r| r.failed().count()).sum::<usize>();
            let mut v = json!({"failed": failed, "sets": reports});
            // a library with many missing samples can report hundreds of problems per instrument
            for set in v["sets"].as_array_mut().into_iter().flatten() {
                for inst in set["instruments"].as_array_mut().into_iter().flatten() {
                    if let Some(d) = inst.get_mut("diagnostics").and_then(Value::as_array_mut).filter(|d| d.len() > 5) {
                        let total = d.len();
                        d.truncate(5);
                        inst["diagnostics_total"] = json!(total);
                    }
                }
            }
            v
        }
        "untracked" => {
            if !names.is_empty() {
                bail!("untracked looks at the output folders of every set: leave `sets` out");
            }
            json!({"untracked": manifest::untracked(&sets)?})
        }
        other => bail!("unknown action '{other}' (check, build or untracked)"),
    };
    out["seconds"] = secs(t0);
    text(out)
}

/// A derived copy of an SFZ instrument: tighten, retune or even (see `flux sfz help`).
fn sfz_derive(a: &Value) -> Result<Vec<Content>> {
    use flux_sfz::derive::{Request, Tool};
    use flux_sfz::edit::SamplePaths;
    let tool = req::<String>(a, "tool")?;
    let allowed: &[&str] = match tool.as_str() {
        "tighten" => &["release_s", "note"],
        "retune" => &["keys", "max_cents", "add_to_tune", "base"],
        "even" => &["release_s", "velocities", "window_s", "max_change_db", "reset_between_notes"],
        other => bail!("unknown tool '{other}' (tighten, retune or even)"),
    };
    let common = ["tool", "source", "out", "sample_paths"];
    if let Some(k) = a.as_object().into_iter().flat_map(|m| m.keys()).find(|k| !common.contains(&k.as_str()) && !allowed.contains(&k.as_str())) {
        bail!("`{k}` is not an option of {tool}");
    }
    let sample_paths = match get::<String>(a, "sample_paths")?.as_deref() {
        None | Some("relative") => SamplePaths::Relative,
        Some("absolute") => SamplePaths::Absolute,
        Some(x) => bail!("sample_paths must be \"relative\" or \"absolute\", not \"{x}\""),
    };
    let tool = match tool.as_str() {
        "tighten" => Tool::Tighten(flux_sfz::tighten::TightenOptions {
            release_s: req(a, "release_s").context("tighten needs release_s")?,
            note: get(a, "note")?,
            sample_paths,
        }),
        "retune" => {
            let mut o = flux_sfz::retune::RetuneOptions { sample_paths, ..Default::default() };
            if let Some(k) = get::<Vec<pitch::PitchSpec>>(a, "keys")? {
                let [lo, hi] = k.as_slice() else { bail!("keys: give [lowest, highest]") };
                let (lo, hi) = (lo.resolve()?, hi.resolve()?);
                if lo > hi {
                    bail!("keys: the lowest key comes first");
                }
                o.keys = Some((lo, hi));
            }
            if let Some(c) = get(a, "max_cents")? {
                o.max_cents = c;
            }
            o.keep_existing_tune = get(a, "add_to_tune")?.unwrap_or(false);
            Tool::Retune(o)
        }
        _ => {
            let mut o = flux_sfz::even::EvenOptions { sample_paths, release_s: get(a, "release_s")?, ..Default::default() };
            if let Some(v) = get(a, "velocities")? {
                o.velocities = v;
            }
            if let Some(w) = get(a, "window_s")? {
                o.window_s = w;
            }
            if let Some(m) = get(a, "max_change_db")? {
                o.max_change_db = m;
            }
            o.reset_between_notes = get(a, "reset_between_notes")?.unwrap_or(true);
            Tool::Even(o)
        }
    };
    let request = Request {
        tool,
        source: paths::resolve(&req::<String>(a, "source")?),
        out: get::<String>(a, "out")?.map(|o| paths::resolve(&o)),
        base: get::<String>(a, "base")?.map(|b| paths::resolve(&b)),
    };
    let t0 = Instant::now();
    let outcome = flux_sfz::derive::run(paths::root(), &request)?;
    text(json!({
        "written": outcome.written.iter().map(|p| rel(p)).collect::<Vec<_>>(),
        "summary": outcome.report.summary(paths::root()),
        "seconds": secs(t0),
    }))
}

// ---------------------------------------------------------------------------------------------
// render / analyze / export

fn render_summary(st: &State) -> Result<Value> {
    let r = st.render.as_ref().ok_or_else(|| anyhow!("nothing rendered yet"))?;
    let tracks: Vec<Value> = r
        .out
        .tracks
        .iter()
        .map(|t| {
            let mut o = json!({"name": t.name, "engine": t.engine, "peak_dbfs": r1(t.peak_dbfs)});
            if t.bus {
                o["bus"] = json!(true);
            }
            if t.layer != "base" {
                o["layer"] = json!(t.layer);
            }
            if let Some(m) = r.project.tracks.iter().find(|x| x.id == t.id).map(|x| &x.mixer) {
                dynamics_json(m, t, &mut o);
            }
            if let Some(v) = &t.restricted {
                o["license"] = json!(format!("{v}: may only leave Flux mixed with other instruments (never as its own stem)"));
            }
            if let Some(w) = &t.warning {
                o["warning"] = json!(w);
            }
            o
        })
        .collect();
    let mut o = json!({
        "revision": r.revision, "stale": r.revision != st.session.revision, "render_seconds": r1(r.out.render_seconds),
        "loudness": analysis::loudness(&r.out), "sections": analysis::sections(&r.out), "tracks": tracks,
        "master_chain": master_chain_json(&r.project, &r.out),
    });
    if !r.out.layers.is_empty() {
        o["layers"] = json!(r.out.layers.iter().map(|l| &l.0).collect::<Vec<_>>());
        o["layer_sum_error_db"] = r.out.layer_sum_error_db.map(r1).unwrap_or(Value::Null);
        let exposed = r.out.exposed_layers();
        if !exposed.is_empty() {
            o["layers_not_exportable"] = json!(exposed.into_iter().map(|e| e.1).collect::<Vec<_>>());
        }
    }
    if !r.out.warnings.is_empty() {
        o["warnings"] = json!(r.out.warnings);
    }
    Ok(o)
}

/// A track's level-dependent units in a render: the loud level each relative threshold counts from (dBFS) and the
/// threshold that makes, so an agent can see whether a compressor, dynamic EQ band or de-esser acts at all.
fn dynamics_json(m: &Mixer, t: &flux_engine::render::TrackStat, o: &mut Value) {
    if let (Some(c), Some(loud)) = (&m.comp, t.comp_loud_db)
        && c.relative
    {
        o["comp"] = json!({"loud_db": r1(loud), "threshold_dbfs": r1(loud + c.threshold_db)});
    }
    if !t.dyn_eq_loud_db.is_empty() {
        o["dyn_eq"] = json!(m
            .dyn_eq
            .iter()
            .zip(&t.dyn_eq_loud_db)
            .map(|(b, &loud)| json!({"hz": b.hz, "loud_db": r1(loud), "threshold_dbfs": r1(if b.relative { loud + b.threshold_db } else { b.threshold_db })}))
            .collect::<Vec<_>>());
    }
    if let (Some(d), Some(loud)) = (&m.deess, t.deess_loud_db) {
        o["deess"] = json!({"hz": d.hz, "loud_db": r1(loud), "threshold_dbfs": r1(loud + d.threshold_db)});
    }
}

/// The edges of the multiband compressor's bands, low to high (Hz).
pub(crate) fn multiband_edges(mb: &flux_core::Multiband) -> Vec<(f64, f64)> {
    split_edges(&mb.crossovers_hz)
}

/// The edges of the bands a split at `crossovers_hz` makes, low to high (Hz).
pub(crate) fn split_edges(crossovers_hz: &[f64]) -> Vec<(f64, f64)> {
    let mut e = vec![20.0];
    e.extend(crossovers_hz);
    e.push(20000.0);
    e.windows(2).map(|w| (w[0], w[1])).collect()
}

/// What the master's dynamics did in a render: the loudness drive, the limiter's largest gain reduction and the
/// ceiling it settled on, and the loud level each relative stage counted from with the threshold it makes (dBFS):
/// the glue, the dynamic EQ bands, the de-esser and the multiband bands; and each exciter band's loud level.
pub(crate) fn master_chain_json(p: &Project, out: &flux_engine::RenderOutput) -> Value {
    let m = &p.master;
    let mut o = json!({"drive_db": r1(out.drive_db), "limiter_max_gr_db": r1(out.limiter_gr_db), "limiter_ceiling_dbtp": r1(out.limiter_ceiling_db)});
    if let Some(g) = &m.glue {
        o["glue"] = match (g.relative, out.glue_loud_db) {
            (true, Some(loud)) => json!({"loud_db": r1(loud), "threshold_dbfs": r1(loud + g.threshold_db), "ratio": g.ratio}),
            _ => json!({"threshold_dbfs": r1(g.threshold_db), "ratio": g.ratio}),
        };
    }
    if let Some(mb) = &m.multiband {
        o["multiband"] = json!(multiband_edges(mb)
            .iter()
            .zip(&mb.bands)
            .enumerate()
            .map(|(i, ((lo, hi), b))| {
                let mut x = json!({"band": format!("{lo:.0}-{hi:.0} Hz"), "ratio": b.ratio});
                if let Some(&loud) = out.master_loud.multiband.get(i) {
                    x["loud_db"] = r1(loud);
                    x["threshold_dbfs"] = r1(loud + b.threshold_db);
                }
                x
            })
            .collect::<Vec<_>>());
    }
    if !m.dyn_eq.is_empty() {
        o["dyn_eq"] = json!(m
            .dyn_eq
            .iter()
            .enumerate()
            .map(|(i, b)| match out.master_loud.dyn_eq.get(i) {
                Some(&loud) if b.relative => json!({"hz": b.hz, "loud_db": r1(loud), "threshold_dbfs": r1(loud + b.threshold_db)}),
                _ => json!({"hz": b.hz, "threshold_dbfs": r1(b.threshold_db)}),
            })
            .collect::<Vec<_>>());
    }
    if let (Some(d), Some(loud)) = (&m.deess, out.master_loud.deess) {
        o["deess"] = json!({"hz": d.hz, "loud_db": r1(loud), "threshold_dbfs": r1(loud + d.threshold_db)});
    }
    if let Some(e) = &m.exciter {
        o["exciter"] = json!(split_edges(&e.crossovers_hz)
            .iter()
            .zip(&e.bands)
            .enumerate()
            .map(|(i, ((lo, hi), b))| {
                let mut x = json!({"band": format!("{lo:.0}-{hi:.0} Hz"), "mix": b.mix});
                if let Some(&loud) = out.master_loud.exciter.get(i) {
                    x["loud_db"] = r1(loud);
                }
                x
            })
            .collect::<Vec<_>>());
    }
    o
}

pub(crate) fn ensure_render(sh: &Arc<Shared>, timeout: f64) -> Result<()> {
    sh.start_render(None)?;
    if !sh.wait_render(Duration::from_secs_f64(timeout)) {
        bail!("render still running after {timeout} s");
    }
    if let RenderStatus::Failed { error, .. } = &sh.lock().status {
        bail!("render failed: {error}");
    }
    Ok(())
}

fn render(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let stems = get::<String>(a, "stems_dir")?.map(|s| paths::resolve(&s));
    let wait = get::<bool>(a, "wait")?.unwrap_or(true);
    let rev = sh.start_render_with(stems, get::<bool>(a, "layers")?.unwrap_or(false))?;
    if !wait {
        return text(json!({"started": rev, "hint": "render(wait=true) or get_project shows progress"}));
    }
    let timeout = get::<f64>(a, "timeout_s")?.unwrap_or(900.0);
    if !sh.wait_render(Duration::from_secs_f64(timeout)) {
        return text(json!({"still_rendering": rev}));
    }
    let st = sh.lock();
    if let RenderStatus::Failed { error, .. } = &st.status {
        bail!("render failed: {error}");
    }
    text(render_summary(&st)?)
}

fn analyze(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (r, current) = {
        let st = sh.lock();
        (st.render.clone().ok_or_else(|| anyhow!("nothing rendered yet: call render first"))?, st.session.revision)
    };
    let flag = |k: &str, d: bool| get::<bool>(a, k).ok().flatten().unwrap_or(d);
    let o = &r.out;
    let mut m = Map::new();
    m.insert("revision".into(), json!(r.revision));
    m.insert("stale".into(), json!(r.revision != current));
    m.insert("loudness".into(), analysis::loudness(o));
    if flag("sections", true) {
        m.insert("sections".into(), analysis::sections(o));
    }
    if flag("balance", true) {
        m.insert("balance".into(), analysis::balance(o));
    }
    if flag("bands", true) {
        m.insert("bands".into(), analysis::bands(o));
    }
    if flag("bars", false) {
        m.insert("bars".into(), analysis::bars(o));
    }
    let mut warn = o.warnings.clone();
    if o.limiter_gr_db < -4.0 {
        warn.push(format!("limiter reduces up to {:.1} dB: tame the peakiest tracks (comp / lower gain) or lower target_lufs", -o.limiter_gr_db));
    }
    if o.true_peak_dbtp > r.project.master.ceiling_dbtp + 0.05 {
        warn.push(format!("true peak {:.2} dBTP is above the ceiling", o.true_peak_dbtp));
    }
    // the inserts that can keep a loop from repeating exactly (the linear ones fold and always do)
    let suspects = if o.lead_in_inserts.is_empty() {
        "a time-varying strip effect".to_string()
    } else {
        format!("inserts that aren't linear: {}", o.lead_in_inserts.join(", "))
    };
    match (o.seam_error_db, o.seam_crossfade_ms) {
        (Some(s), Some(ms)) => warn.push(format!(
            "loop seam: the loop isn't periodic ({s:.1} dB; suspects: {suspects}), so its first {ms:.0} ms fade in from what follows its end and the wrap doesn't click"
        )),
        (Some(s), None) if s > -60.0 => warn.push(format!("loop seam error {s:.1} dB: something is not loop-safe (suspects: {suspects})")),
        _ => {}
    }
    if (o.lufs - r.project.master.target_lufs).abs() > 0.5 {
        warn.push(format!("integrated loudness {:.1} LUFS misses the target {:.1}", o.lufs, r.project.master.target_lufs));
    }
    // silent tracks that only key a sidechain duck are fine
    let keys: Vec<&str> = r
        .project
        .tracks
        .iter()
        .filter(|t| matches!(t.instrument, Instrument::None) && r.project.tracks.iter().any(|u| u.mixer.duck.is_some_and(|d| d.source_track == t.id)))
        .map(|t| t.name.as_str())
        .collect();
    for t in &o.tracks {
        if let Some(w) = &t.warning {
            warn.push(format!("{}: {w}", t.name));
        } else if t.audible && t.peak_dbfs < -90.0 && !keys.contains(&t.name.as_str()) {
            warn.push(format!("{}: silent (no instrument, out-of-range notes, or needs a CC?)", t.name));
        }
        if let Some(w) = timbre_warning(t) {
            warn.push(format!("{}: {w}", t.name));
        }
    }
    m.insert("warnings".into(), json!(warn));
    text(Value::Object(m))
}

/// Same thresholds as the instrument report cards (tools/instrument_card.py): the listener rejected bright, spiky
/// plucks (lyre hi3k -28.5 dB, zither attack 17.9 dB) and liked the soft VSCO harp (hi3k -49.6, attack 5.8).
/// Percussion layers are expected to be sharp and are skipped.
const HARSH_HI3K_DB: f64 = -35.0;
const SHARP_ATTACK_DB: f64 = 12.0;

fn timbre_warning(t: &flux_engine::render::TrackStat) -> Option<String> {
    let tb = t.timbre.as_ref()?;
    if !t.audible || t.peak_dbfs < -60.0 || matches!(t.layer.as_str(), "percussion" | "perc" | "drums") {
        return None;
    }
    let attack = tb.attack_db.unwrap_or(0.0);
    if attack > SHARP_ATTACK_DB {
        Some(format!("sharp attack {attack:.1} dB (> {SHARP_ATTACK_DB}; the rejected zither measured 17.9): soften with a slower \
compressor attack, lower velocities, an LPF, or a softer instrument"))
    } else if tb.hi3k_db > HARSH_HI3K_DB && attack > 6.0 {
        Some(format!("harsh: {:.1} dB above 3 kHz on a plucked/struck sound (> {HARSH_HI3K_DB}; the liked harp is -49.6): lower the LPF \
or cut the highs", tb.hi3k_db))
    } else if tb.hi3k_db > -20.0 {
        Some(format!("very bright: {:.1} dB above 3 kHz (bowed strings, reeds and brass usually sit between -30 and -10)", tb.hi3k_db))
    } else {
        None
    }
}

/// Write audio in an export format. wav16 and FLAC are 16-bit with TPDF dither (noise-shaped with `shaped`); FLAC is
/// tagged with `title` and, for loops, with LOOPSTART / LOOPLENGTH over the whole file; every file is decoded again
/// and its MD5 checked.
fn write_audio(format: &str, sr: u32, buf: &(Vec<f32>, Vec<f32>), path: &Path, title: &str, looped: bool, shaped: bool) -> Result<()> {
    use flux_assets::flac::{Dither, FlacOptions};
    match format {
        "wav16" if shaped => flux_engine::render::write_wav_i16(path, sr, &flux_engine::dither::to_i16_shaped(buf, 1)),
        "wav16" => flux_engine::render::write_wav_i16(path, sr, &flux_engine::dsp::to_i16_tpdf(buf, 1)),
        "wav32" => flux_engine::render::write_wav_f32(path, sr, buf),
        _ => {
            let dither = if shaped { Dither::Shaped { seed: 1 } } else { Dither::Tpdf { seed: 1 } };
            let mut opts = FlacOptions { dither, ..FlacOptions::default() }.tag("TITLE", title);
            if looped {
                opts = opts.looped(0, buf.0.len() as u64);
            }
            flux_assets::flac::write_f32_stereo(path, sr, &buf.0, &buf.1, &opts).map(|_| ())
        }
    }
}

fn export(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let format = get::<String>(a, "format")?.unwrap_or("wav16".into());
    let ext = match format.as_str() {
        "wav16" | "wav32" => "wav",
        "flac" => "flac",
        f => bail!("unknown format '{f}' (wav16, wav32, flac)"),
    };
    let layers = get::<bool>(a, "layers")?.unwrap_or(false);
    let shaped = match get::<String>(a, "dither")?.as_deref() {
        None | Some("tpdf") => false,
        Some("shaped") => true,
        Some(d) => bail!("unknown dither '{d}' (tpdf, shaped)"),
    };
    let (stale, has_layers) = {
        let st = sh.lock();
        (st.render.as_ref().map(|r| r.revision) != Some(st.session.revision), st.render.as_ref().is_some_and(|r| !r.out.layers.is_empty()))
    };
    if stale {
        if get::<bool>(a, "render_if_stale")?.unwrap_or(true) {
            ensure_render_with(sh, 900.0, layers)?;
        } else if sh.lock().render.is_none() {
            bail!("nothing rendered yet");
        } else if layers && !has_layers {
            bail!("the last render has no layer stems and render_if_stale=false");
        }
    } else if layers && !has_layers {
        ensure_render_with(sh, 900.0, true)?;
    }
    let (r, name) = {
        let st = sh.lock();
        (st.render.clone().ok_or_else(|| anyhow!("nothing rendered"))?, st.session.project.name.clone())
    };
    if layers {
        // before writing anything, so a refused export leaves no partial set of files
        r.out.exportable_layers()?;
    }
    let path = match get::<String>(a, "path")? {
        Some(p) => paths::resolve(&p),
        None => paths::exports_dir().join(slug(&name)).join(format!("{}_flux.{ext}", slug(&name))),
    };
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    write_audio(&format, r.out.sr, &(r.out.left.clone(), r.out.right.clone()), &path, &name, r.out.looped, shaped)?;
    let bytes = std::fs::metadata(&path)?.len();
    let dur = r.out.duration_s();
    let mut report = json!({
        "path": rel(&path), "format": format, "bytes": bytes,
        "dither": if format == "wav32" { Value::Null } else { json!(if shaped { "shaped" } else { "tpdf" }) }, "mb": r1(bytes as f64 / 1e6), "revision": r.revision,
        "duration_s": r1(dur), "loudness": analysis::loudness(&r.out),
        "roblox": {"under_20mb": bytes < 20_000_000, "under_7min": dur < 420.0, "sample_rate_ok": r.out.sr <= 48000},
    });
    if layers {
        report["layers"] = export_layers(&r, &format, &path, shaped)?;
    }
    // the file also gets the bar lines, for game_audio_check's loop-seam test
    let mut file_report = report.clone();
    let sr = r.out.sr as f64;
    file_report["bar_starts_s"] = json!(r.out.bar_starts.iter().map(|&s| (s as f64 / sr * 1e4).round() / 1e4).collect::<Vec<_>>());
    std::fs::write(path.with_extension("report.json"), serde_json::to_string_pretty(&file_report)?)?;
    sh.lock().session.note(Source::Agent, format!("exported {}", rel(&path)));
    text(report)
}

/// One file per layer beside the master (`<base>_<layer>.<ext>`) and a `<base>.layers.json` manifest for the game.
fn export_layers(r: &crate::state::LastRender, format: &str, master: &Path, shaped: bool) -> Result<Value> {
    let o = &r.out;
    let layers = o.exportable_layers()?;
    if layers.is_empty() {
        bail!("the render has no layer stems");
    }
    let stem = master.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = master.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    let dir = master.parent().unwrap_or(Path::new("."));
    let mut files = vec![];
    for (name, buf) in layers {
        let f = dir.join(format!("{stem}_{}.{ext}", slug(name)));
        write_audio(format, o.sr, buf, &f, &format!("{} ({name})", r.project.name), o.looped, shaped)?;
        let peak = buf.0.iter().chain(buf.1.iter()).fold(0f32, |m, x| m.max(x.abs()));
        let lufs = flux_engine::render::measure(&buf.0, &buf.1, o.sr).map(|m| m.0).unwrap_or(f64::NEG_INFINITY);
        let tracks: Vec<&str> = r.project.tracks.iter().filter(|t| t.layer_name() == name).map(|t| t.name.as_str()).collect();
        files.push(json!({"layer": name, "file": f.file_name().map(|s| s.to_string_lossy().to_string()),
            "lufs": r1(lufs), "peak_dbfs": r1(20.0 * (peak as f64 + 1e-12).log10()), "bytes": std::fs::metadata(&f)?.len(), "tracks": tracks}));
    }
    let p = &r.project;
    let tm = TimeMap::new(p);
    let sec = |s: usize| (s as f64 / o.sr as f64 * 1000.0).round() / 1000.0;
    let manifest = json!({
        "project": p.name, "master": master.file_name().map(|s| s.to_string_lossy().to_string()),
        "sample_rate": o.sr, "duration_s": sec(o.left.len()), "looped": o.looped,
        "loop_s": if o.looped { json!(sec(o.loop_samples)) } else { Value::Null },
        "tempo": p.tempo, "time_signature": p.time_signature, "bars": p.bars,
        "bar_s": tm.sec(tm.beats_per_bar()) - tm.sec(0.0),
        "sections": o.sections.iter().map(|s| json!({"name": s.name, "start_bar": s.start_bar, "bars": s.bars,
            "start_s": sec(s.start), "end_s": sec(s.end)})).collect::<Vec<_>>(),
        "layers": files,
        "sum_error_db": o.layer_sum_error_db.map(r1).unwrap_or(Value::Null),
        "usage": "Start every layer at the same moment and loop them together; their sum is the master. Fade a layer's volume to \
bring it in or out (e.g. percussion only in combat). Section times are for jumping to or waiting for a section boundary.",
    });
    let mf = dir.join(format!("{stem}.layers.json"));
    std::fs::write(&mf, serde_json::to_string_pretty(&manifest)?)?;
    Ok(json!({"manifest": rel(&mf), "files": files, "sum_error_db": manifest["sum_error_db"]}))
}

pub(crate) fn ensure_render_with(sh: &Arc<Shared>, timeout: f64, layers: bool) -> Result<()> {
    if !layers {
        return ensure_render(sh, timeout);
    }
    if !sh.wait_render(Duration::from_secs_f64(timeout)) {
        bail!("a render is still running after {timeout} s");
    }
    sh.start_render_with(None, true)?;
    if !sh.wait_render(Duration::from_secs_f64(timeout)) {
        bail!("render still running after {timeout} s");
    }
    if let RenderStatus::Failed { error, .. } = &sh.lock().status {
        bail!("render failed: {error}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// transport / comments / auditions / activity / gui

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Live => "live",
        Mode::Render => "render",
        Mode::Clip => "audition clip",
    }
}

fn transport(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let action: String = req(a, "action")?;
    match get::<String>(a, "mode")?.as_deref() {
        Some("live") => sh.player.set_mode(Mode::Live),
        Some("render") => sh.player.set_mode(Mode::Render),
        Some(m) => bail!("unknown mode '{m}' (live, render)"),
        None => {}
    }
    let (r, tm) = {
        let st = sh.lock();
        (st.render.clone(), Shared::timeline_map(&st, &sh.player))
    };
    if let Some(bar) = get::<f64>(a, "bar")? {
        let tm = tm.as_ref().ok_or_else(|| anyhow!("nothing rendered yet: render first or use mode=live"))?;
        let b = bar.max(1.0);
        sh.player.seek_seconds(tm.sec(tm.bar_beat(b.floor() as u32) + (b - b.floor()) * tm.beats_per_bar()));
    }
    match action.as_str() {
        "play" => sh.play().map_err(|e| anyhow!(e))?,
        "stop" => sh.player.stop(),
        "seek" | "status" => {}
        x => bail!("unknown action '{x}'"),
    }
    {
        let st = sh.lock();
        Shared::repaint(&st);
    }
    let s = sh.player.seconds();
    let bar = tm.as_ref().map(|tm| r1(tm.beat(s) / tm.beats_per_bar() + 1.0));
    let mut o = json!({"playing": sh.player.is_playing(), "mode": mode_name(sh.player.mode()), "position_s": r1(s), "bar": bar,
                "render_revision": r.as_ref().map(|r| r.revision), "device_rate": sh.player.device_rate()});
    if sh.player.mode() == Mode::Live {
        let loading = sh.player.live_loading();
        if loading > 0 {
            o["loading_instruments"] = json!(loading);
        }
        let names: std::collections::HashMap<u64, String> = sh.lock().session.project.tracks.iter().map(|t| (t.id, t.name.clone())).collect();
        let odd: Vec<Value> = sh
            .player
            .live_status()
            .into_iter()
            .filter(|(_, s)| s != "live" && s != "silent")
            .map(|(id, s)| json!([names.get(&id).cloned().unwrap_or(id.to_string()), s]))
            .collect();
        if !odd.is_empty() {
            o["tracks"] = json!(odd);
        }
    }
    // the human playing in: MIDI inputs, and a recording in progress
    let inputs: Vec<String> = sh.midi.devices().into_iter().filter(|d| d.connected).map(|d| d.name).collect();
    if !inputs.is_empty() {
        o["midi_inputs"] = json!(inputs);
    }
    if let Some(id) = sh.midi.recording() {
        let name = sh.lock().session.project.tracks.iter().find(|t| t.id == id).map(|t| t.name.clone());
        o["recording"] = json!(name.unwrap_or_else(|| id.to_string()));
    }
    text(o)
}

fn comment_json(p: &Project, c: &flux_core::Comment) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), json!(c.id));
    o.insert("bars".into(), json!(match c.to_bar {
        Some(t) if t > c.bar => format!("{}-{t}", c.bar),
        _ => c.bar.to_string(),
    }));
    if c.beat > 0.0 {
        o.insert("beat".into(), json!(c.beat));
    }
    if let Some(t) = c.track {
        o.insert("track".into(), json!(p.tracks.iter().find(|x| x.id == t).map(|x| x.name.clone()).unwrap_or(format!("#{t} (deleted)"))));
    }
    o.insert("author".into(), json!(c.author));
    o.insert("text".into(), json!(c.text));
    if c.resolved {
        o.insert("resolved".into(), json!(true));
    }
    if !c.replies.is_empty() {
        o.insert("replies".into(), json!(c.replies.iter().map(|r| json!({"author": r.author, "text": r.text})).collect::<Vec<_>>()));
    }
    Value::Object(o)
}

fn comments(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let action: String = req(a, "action")?;
    if action == "list" {
        let all = get::<bool>(a, "include_resolved")?.unwrap_or(false);
        let st = sh.lock();
        let p = &st.session.project;
        let rows: Vec<Value> = p.comments.iter().filter(|c| all || !c.resolved).map(|c| comment_json(p, c)).collect();
        return text(json!({"comments": rows, "resolved_hidden": if all { 0 } else { p.comments.iter().filter(|c| c.resolved).count() }}));
    }
    let op: CommentOp = serde_json::from_value(a.clone()).with_context(|| format!("comments action '{action}'"))?;
    let r = sh.comment(&op, Source::Agent)?;
    text(json!({"ok": true, "result": r}))
}

fn audition_json(a: &Audition) -> Value {
    json!({
        "id": a.id, "label": a.label, "question": a.question, "bars": format!("{}-{}", a.from_bar, a.to_bar), "revision": a.revision,
        "variants": a.variants.iter().enumerate().map(|(i, v)| json!({"variant": letter(i).to_string(), "name": v.name,
            "edits": v.commands.len(), "lufs_before_matching": r1(v.lufs), "file": rel(&v.path)})).collect::<Vec<_>>(),
        "pick": a.pick.map(|i| format!("{} ({})", letter(i), a.variants[i].name)), "note": a.note, "applied": a.applied,
    })
}

fn audition_index(st: &State, a: &Value) -> Result<usize> {
    match get::<u64>(a, "id")? {
        Some(id) => st.auditions.iter().position(|x| x.id == id).ok_or_else(|| anyhow!("no audition {id}")),
        None => st.auditions.len().checked_sub(1).ok_or_else(|| anyhow!("no auditions yet")),
    }
}

/// Which variant `variant` names (letter, name or index), else the pick.
fn variant_index(au: &Audition, a: &Value) -> Result<usize> {
    let v = a.get("variant").cloned().unwrap_or(Value::Null);
    let i = match &v {
        Value::Null => au.pick.ok_or_else(|| anyhow!("audition {} has no pick yet: wait for the human or name a variant", au.id))?,
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("bad variant index"))? as usize,
        Value::String(s) => {
            let t = s.trim();
            let by_letter = (t.len() == 1).then(|| t.to_ascii_uppercase().as_bytes()[0]).filter(|c| c.is_ascii_uppercase()).map(|c| (c - b'A') as usize);
            match au.variants.iter().position(|x| x.name.eq_ignore_ascii_case(t)).or(by_letter) {
                Some(i) => i,
                None => bail!("no variant '{t}'"),
            }
        }
        _ => bail!("variant must be a letter, name or index"),
    };
    if i >= au.variants.len() {
        bail!("audition {} has {} variants", au.id, au.variants.len());
    }
    Ok(i)
}

fn audition(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let action: String = req(a, "action")?;
    match action.as_str() {
        "create" => audition_create(sh, a),
        "list" => {
            let st = sh.lock();
            text(json!({"auditions": st.auditions.iter().map(audition_json).collect::<Vec<_>>()}))
        }
        "wait" => {
            let timeout = get::<f64>(a, "timeout_s")?.unwrap_or(600.0);
            let deadline = Instant::now() + Duration::from_secs_f64(timeout);
            let mut st = sh.lock();
            let i = audition_index(&st, a)?;
            while st.auditions[i].pick.is_none() {
                if sh.changed.wait_until(&mut st, deadline).timed_out() {
                    return text(json!({"picked": false, "timed_out_s": timeout, "audition": audition_json(&st.auditions[i])}));
                }
            }
            text(json!({"picked": true, "audition": audition_json(&st.auditions[i])}))
        }
        "play" => {
            let clip = {
                let st = sh.lock();
                let au = &st.auditions[audition_index(&st, a)?];
                au.variants[variant_index(au, a)?].clip.clone()
            };
            sh.player.play_clip(clip).map_err(|e| anyhow!(e))?;
            text(json!({"ok": true}))
        }
        "apply" => {
            let (id, want) = {
                let st = sh.lock();
                let au = &st.auditions[audition_index(&st, a)?];
                (au.id, variant_index(au, a)?)
            };
            let before = sh.lock().session.revision;
            let (i, results, rev) = sh.apply_audition(id, Some(want), Source::Agent)?;
            let st = sh.lock();
            let name = st.auditions.iter().find(|x| x.id == id).map(|x| x.variants[i].name.clone()).unwrap_or_default();
            let mut o = json!({"ok": true, "applied": format!("{} ({name})", letter(i)), "results": results, "revision": st.session.revision});
            if before != rev {
                o["note"] = json!(format!("the project changed since the audition (revision {rev} -> {before}); check the result"));
            }
            text(o)
        }
        x => bail!("unknown action '{x}'"),
    }
}

fn parse_commands(raw: Vec<Value>) -> Result<Vec<Command>> {
    raw.into_iter()
        .enumerate()
        .map(|(i, c)| serde_json::from_value::<Command>(c).with_context(|| format!("command {}", i + 1)))
        .collect()
}

fn audition_create(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    #[derive(serde::Deserialize)]
    struct V {
        name: String,
        commands: Vec<Value>,
    }
    let mut vs: Vec<V> = req(a, "variants")?;
    if get::<bool>(a, "include_current")?.unwrap_or(true) {
        vs.insert(0, V { name: "current".into(), commands: vec![] });
    }
    if vs.len() < 2 {
        bail!("an audition needs at least two variants");
    }
    if vs.len() > 7 {
        bail!("at most 6 variants plus the current project");
    }
    let (base, revision) = {
        let st = sh.lock();
        (st.session.project.clone(), st.session.revision)
    };
    let from_bar: u32 = req(a, "from_bar")?;
    let to_bar = get::<u32>(a, "to_bar")?.unwrap_or(from_bar + 7).min(base.bars);
    if from_bar < 1 || from_bar > to_bar {
        bail!("bars {from_bar}-{to_bar} are not inside the song (1-{})", base.bars);
    }
    let label = get::<String>(a, "label")?.unwrap_or_else(|| format!("bars {from_bar}-{to_bar}"));
    let question = get::<String>(a, "question")?.unwrap_or_default();
    // make every variant's project first, so a bad command fails before any rendering
    let mut projects = vec![];
    for v in &vs {
        let mut p = base.clone();
        for (k, c) in parse_commands(v.commands.clone()).with_context(|| format!("variant '{}'", v.name))?.iter().enumerate() {
            flux_core::commands::apply(&mut p, c).with_context(|| format!("variant '{}', command {}", v.name, k + 1))?;
        }
        projects.push(p);
    }
    let id = sh.lock().auditions.iter().map(|x| x.id).max().unwrap_or(0) + 1;
    let dir = paths::root().join("build").join("auditions").join(slug(&base.name));
    std::fs::create_dir_all(&dir)?;
    let t0 = Instant::now();
    let mut cuts = vec![];
    for (i, (v, p)) in vs.iter().zip(&projects).enumerate() {
        {
            let mut st = sh.lock();
            st.session.note(Source::System, format!("audition {id}: rendering {} '{}'", letter(i), v.name));
            Shared::repaint(&st);
        }
        let out = flux_engine::render(p, &flux_engine::RenderOptions::default()).with_context(|| format!("rendering variant '{}'", v.name))?;
        let tm = TimeMap::new(p);
        let sr = out.sr as f64;
        let a0 = ((tm.sec(tm.bar_beat(from_bar)) * sr) as usize).min(out.left.len());
        let b0 = ((tm.sec(tm.bar_beat(to_bar + 1)) * sr) as usize).clamp(a0, out.left.len());
        let (mut l, mut r) = (out.left[a0..b0].to_vec(), out.right[a0..b0].to_vec());
        let fade_in = (0.01 * sr) as usize;
        let fade_out = (0.08 * sr) as usize;
        let n = l.len();
        for k in 0..n.min(fade_in) {
            let g = k as f32 / fade_in as f32;
            l[k] *= g;
            r[k] *= g;
        }
        for k in 0..n.min(fade_out) {
            let g = k as f32 / fade_out as f32;
            l[n - 1 - k] *= g;
            r[n - 1 - k] *= g;
        }
        let lufs = flux_engine::render::measure(&l, &r, out.sr).map(|m| m.0).unwrap_or(f64::NEG_INFINITY);
        cuts.push((l, r, lufs, out.sr));
    }
    // match loudness by turning the louder ones down to the quietest (never up, so nothing clips)
    let target = cuts.iter().map(|c| c.2).filter(|x| x.is_finite()).fold(f64::INFINITY, f64::min);
    let mut variants = vec![];
    for (i, (v, (mut l, mut r, lufs, sr))) in vs.into_iter().zip(cuts).enumerate() {
        if target.is_finite() && lufs.is_finite() {
            let g = 10f64.powf((target - lufs) / 20.0) as f32;
            l.iter_mut().chain(r.iter_mut()).for_each(|x| *x *= g);
        }
        let path = dir.join(format!("{id:03}_{}_{}.wav", letter(i), slug(&v.name)));
        flux_engine::render::write_wav_f32(&path, sr, &(l.clone(), r.clone()))?;
        let key = format!("audition {id} / {}", letter(i));
        variants.push(Variant { name: v.name, commands: v.commands, clip: Arc::new(Clip { sr, left: l, right: r, key }), path, lufs });
    }
    let au = Audition { id, label, question, from_bar, to_bar, revision, variants, pick: None, note: String::new(), applied: false };
    let summary = audition_json(&au);
    {
        let mut st = sh.lock();
        let names: Vec<String> = au.variants.iter().enumerate().map(|(i, v)| format!("{} '{}'", letter(i), v.name)).collect();
        st.session.note(Source::Agent, format!("audition {id} ({}): {}", au.label, names.join(", ")));
        st.auditions.push(au);
        st.audition_rev += 1;
        if st.gui_open {
            st.focus = Some(Focus { track: None, bar: Some(from_bar), panel: Some("auditions") });
        }
        Shared::repaint(&st);
        sh.changed.notify_all();
    }
    text(json!({"ok": true, "render_seconds": r1(t0.elapsed().as_secs_f64()), "audition": summary,
        "next": "the human picks in the GUI's Auditions tab; audition action=wait blocks until they do, then action=apply"}))
}

fn activity(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let since = get::<u64>(a, "since_revision")?;
    let limit = get::<usize>(a, "limit")?.unwrap_or(40);
    let src = get::<String>(a, "source")?.unwrap_or("any".into());
    let st = sh.lock();
    let items: Vec<&flux_core::session::LogEntry> = st
        .session
        .log
        .iter()
        .filter(|e| since.is_none_or(|s| e.revision > s))
        .filter(|e| src == "any" || serde_json::to_value(e.source).ok().and_then(|v| v.as_str().map(|s| s == src)).unwrap_or(false))
        .collect();
    let skip = items.len().saturating_sub(limit);
    let rows: Vec<Value> = items
        .into_iter()
        .skip(skip)
        .map(|e| json!({"revision": e.revision, "source": e.source, "at_s": r1(e.at), "summary": e.summary, "ok": e.ok}))
        .collect();
    text(json!({"revision": st.session.revision, "entries": rows}))
}

const PANELS: [&str; 8] = ["arrange", "piano_roll", "mixer", "track", "master", "analysis", "comments", "auditions"];

fn gui(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let action: String = req(a, "action")?;
    match action.as_str() {
        "open" => {
            if sh.lock().gui_open {
                return text(json!({"ok": true, "note": "already open"}));
            }
            if !sh.send_main(MainMsg::OpenGui) {
                bail!("this Flux process cannot show a window");
            }
            let t0 = Instant::now();
            while !sh.lock().gui_open && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(50));
            }
            text(json!({"ok": sh.lock().gui_open}))
        }
        "focus" => {
            let mut st = sh.lock();
            let track = match get::<TrackRef>(a, "track")? {
                Some(r) => {
                    let p = &st.session.project;
                    Some(p.tracks[p.track_index(&r).ok_or_else(|| anyhow!("no track {r}"))?].id)
                }
                None => None,
            };
            let panel = match get::<String>(a, "panel")? {
                Some(p) => Some(*PANELS.iter().find(|x| **x == p).ok_or_else(|| anyhow!("unknown panel {p:?}; one of {}", PANELS.join(", ")))?),
                None => None,
            };
            st.focus = Some(Focus { track, bar: get(a, "bar")?, panel });
            Shared::repaint(&st);
            text(json!({"ok": true, "gui_open": st.gui_open}))
        }
        "screenshot" => {
            {
                let mut st = sh.lock();
                if !st.gui_open {
                    bail!("the GUI is not open (gui action=open)");
                }
                st.screenshot = None;
                st.screenshot_wanted = true;
                Shared::repaint(&st);
            }
            let deadline = Instant::now() + Duration::from_secs(8);
            let img = {
                let mut st = sh.lock();
                while st.screenshot.is_none() {
                    if sh.changed.wait_until(&mut st, deadline).timed_out() {
                        break;
                    }
                }
                st.screenshot.take()
            };
            let img = img.ok_or_else(|| anyhow!("the GUI did not deliver a screenshot (minimised window?)"))?;
            let png = encode_png(&img, get::<usize>(a, "max_width")?.unwrap_or(1600).max(200))?;
            Ok(vec![Content::Png(png), Content::Text(format!("screenshot {}x{}", img.size[0], img.size[1]))])
        }
        x => bail!("unknown action '{x}'"),
    }
}

/// PNG-encode a screenshot, box-downscaled so it is at most `max_w` pixels wide.
fn encode_png(img: &eframe::egui::ColorImage, max_w: usize) -> Result<Vec<u8>> {
    let [w, h] = img.size;
    let f = w.div_ceil(max_w).max(1);
    let (ow, oh) = (w / f, h / f);
    let mut rgb = Vec::with_capacity(ow * oh * 3);
    for y in 0..oh {
        for x in 0..ow {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for dy in 0..f {
                for dx in 0..f {
                    let c = img.pixels[(y * f + dy) * w + x * f + dx];
                    r += c.r() as u32;
                    g += c.g() as u32;
                    b += c.b() as u32;
                }
            }
            let n = (f * f) as u32;
            rgb.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8]);
        }
    }
    let mut bytes = vec![];
    {
        let mut enc = png::Encoder::new(&mut bytes, ow as u32, oh as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header()?;
        wr.write_image_data(&rgb)?;
        wr.finish()?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{Channel, Comp, DeEss, DynBand, EqBand, EqKind, EqPhase, Exciter, Imager, Master, MbBand, Multiband};
    use std::collections::BTreeSet;

    /// The property names of an object schema, or of the object in a nullable one (`anyOf` with null).
    fn keys(s: &Value) -> BTreeSet<String> {
        let o = s["anyOf"].as_array().and_then(|a| a.iter().find(|x| x["type"] == "object")).unwrap_or(s);
        o["properties"].as_object().unwrap_or_else(|| panic!("not an object schema: {s}")).keys().cloned().collect()
    }

    /// The fields a value serializes with.
    fn fields(v: impl serde::Serialize) -> BTreeSet<String> {
        serde_json::to_value(v).unwrap().as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn the_master_schema_names_every_master_setting() {
        // every optional setting set away from its default, so none is skipped when serialized
        let band = EqBand { slope: 24, channel: Channel::Side, ..EqBand::new(EqKind::HighPass, 30.0, 0.0, 0.7) };
        let dyn_band = DynBand { channel: Channel::Mid, ..DeEss::default().band() };
        let mb_band = MbBand { knee_db: 12.0, mix: 0.5, channel: Channel::Mid, ..MbBand::new(-3.0, 2.0, 15.0, 150.0) };
        let mb = Multiband { bands: vec![mb_band; 4], ..Multiband::default() };
        let m = Master {
            glue: Some(Comp::default()),
            eq: vec![band],
            eq_phase: EqPhase::Linear,
            dyn_eq: vec![dyn_band],
            deess: Some(DeEss::default()),
            multiband: Some(mb.clone()),
            exciter: Some(Exciter::default()),
            imager: Some(Imager::default()),
            mono_below_hz: 120.0,
            width: 1.1,
            ..Master::default()
        };
        let s = master_s();
        let mut want = fields(&m);
        // set with add_insert and the automation commands
        for elsewhere in ["inserts", "reverb_inserts", "automation"] {
            want.remove(elsewhere);
        }
        assert_eq!(keys(&s), want, "master_s() and Master differ");
        let p = &s["properties"];
        assert_eq!(keys(&p["eq"]["items"]), fields(band));
        assert_eq!(keys(&p["dyn_eq"]["items"]), fields(dyn_band));
        assert_eq!(keys(&p["deess"]), fields(DeEss::default()));
        assert_eq!(keys(&p["multiband"]), fields(&mb));
        let nested = |k: &str, f: &str| keys(&p[k]["anyOf"].as_array().unwrap()[1]["properties"][f]["items"]);
        assert_eq!(nested("multiband", "bands"), fields(mb_band));
        assert_eq!(keys(&p["exciter"]), fields(Exciter::default()));
        assert_eq!(nested("exciter", "bands"), fields(Exciter::default().bands[0]));
        assert_eq!(keys(&p["imager"]), fields(Imager::default()));
        // the track strip shares the EQ and dynamic EQ schemas
        let mixer = &mixer_s()["properties"];
        assert_eq!(mixer["eq"], p["eq"]);
        assert_eq!(mixer["dyn_eq"], p["dyn_eq"]);
    }

    #[test]
    fn every_master_setting_in_the_schema_parses() {
        // the schema's own examples and defaults, as an agent would send them
        let cmd = json!({"op": "set_master", "master": {
            "eq": [{"kind": "high_pass", "hz": 25, "db": 0, "slope": 24}, {"kind": "high_shelf", "hz": 8000, "db": 1.5, "channel": "side"}],
            "eq_phase": "linear",
            "dyn_eq": [{"kind": "peak", "hz": 250, "threshold_db": -4, "channel": "mid"}],
            "deess": {},
            "multiband": {"crossovers_hz": [150, 2500, 8000], "bands": [{"threshold_db": -4, "ratio": 2, "knee_db": 10, "mix": 0.5, "channel": "mid"},
                {"threshold_db": -3, "ratio": 1.6}, {"threshold_db": -3, "ratio": 1.8}, {"threshold_db": -4, "ratio": 2}]},
            "exciter": {},
            "imager": {"crossovers_hz": [150, 2500, 8000], "widths": [0.6, 1.0, 1.1, 1.2]}}});
        let mut p = Project::new("Test", 120.0, 8);
        flux_core::commands::apply(&mut p, &serde_json::from_value::<Command>(cmd).unwrap()).unwrap();
        assert_eq!(p.master.exciter, Some(Exciter::default()));
        assert_eq!(p.master.eq_phase, EqPhase::Linear);
        assert_eq!(p.master.imager.as_ref().map(|i| i.widths.len()), Some(4));
    }
}
