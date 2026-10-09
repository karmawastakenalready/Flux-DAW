//! Every edit to a project is a `Command`. Agents send them as JSON (`{"op": "add_notes", ...}`);
//! the GUI builds the same values. `apply` mutates the project and returns a small JSON result.

use crate::model::*;
use crate::pitch::PitchSpec;
use crate::time::TimeMap;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

fn double_option<'de, T, D>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

fn d_vel() -> u8 {
    90
}

/// A note as an agent writes it: either absolute (`beat` from the top) or `bar` + `beat` within the bar.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteSpec {
    #[serde(default)]
    pub bar: Option<u32>,
    #[serde(default)]
    pub beat: f64,
    pub dur: f64,
    pub pitch: PitchSpec,
    #[serde(default = "d_vel")]
    pub vel: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CcSpec {
    #[serde(default)]
    pub bar: Option<u32>,
    #[serde(default)]
    pub beat: f64,
    pub value: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BendSpec {
    #[serde(default)]
    pub bar: Option<u32>,
    #[serde(default)]
    pub beat: f64,
    pub value: i16,
}

/// Identifies one existing note by where it starts (`bar` + `beat`, or absolute `beat`) and its pitch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteKey {
    #[serde(default)]
    pub bar: Option<u32>,
    #[serde(default, alias = "start")]
    pub beat: f64,
    pub pitch: PitchSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DuckSpec {
    pub source_track: TrackRef,
    pub depth_db: f64,
    pub release_ms: f64,
}

/// Partial update of a channel strip: only the fields present change. `null` clears optional units.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MixerPatch {
    pub gain_db: Option<f64>,
    pub pan: Option<f64>,
    pub width: Option<f64>,
    pub mute: Option<bool>,
    pub solo: Option<bool>,
    pub hpf_hz: Option<f64>,
    pub lpf_hz: Option<f64>,
    pub hpf_slope: Option<u32>,
    pub lpf_slope: Option<u32>,
    pub mono_below_hz: Option<f64>,
    pub eq: Option<Vec<EqBand>>,
    pub drive_db: Option<f64>,
    #[serde(default, deserialize_with = "double_option")]
    pub amp: Option<Option<AmpFx>>,
    #[serde(default, deserialize_with = "double_option")]
    pub chorus: Option<Option<ChorusFx>>,
    pub bitcrush_bits: Option<f64>,
    #[serde(default, deserialize_with = "double_option")]
    pub clip_db: Option<Option<f64>>,
    pub post_gain_db: Option<f64>,
    #[serde(default, deserialize_with = "double_option")]
    pub gate: Option<Option<Gate>>,
    #[serde(default, deserialize_with = "double_option")]
    pub comp: Option<Option<Comp>>,
    #[serde(default, deserialize_with = "double_option")]
    pub delay: Option<Option<DelayFx>>,
    #[serde(default, deserialize_with = "double_option")]
    pub duck: Option<Option<DuckSpec>>,
    pub reverb_send: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MasterPatch {
    #[serde(default, deserialize_with = "double_option")]
    pub glue: Option<Option<Comp>>,
    pub eq: Option<Vec<EqBand>>,
    pub target_lufs: Option<f64>,
    pub ceiling_dbtp: Option<f64>,
    pub reverb: Option<ReverbParams>,
    pub edge_fade_ms: Option<f64>,
    pub tail_s: Option<f64>,
}

/// The two buses that have insert chains (tracks are named by `track`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Bus {
    /// After the reverb return joins the tracks, before the glue compressor.
    Master,
    /// The reverb return: with any insert there, the sends run through the chain instead of the built-in reverb.
    Reverb,
}

fn d_one() -> f64 {
    1.0
}

fn none_instr() -> Instrument {
    Instrument::None
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    SetProject {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        bars: Option<u32>,
        #[serde(default)]
        looped: Option<bool>,
        #[serde(default)]
        time_signature: Option<(u32, u32)>,
        #[serde(default)]
        bpm: Option<f64>,
        #[serde(default)]
        notes: Option<String>,
    },
    SetTempo {
        points: Vec<TempoPoint>,
    },
    SetSections {
        sections: Vec<Section>,
    },
    /// Replace the chord track.
    SetHarmony {
        chords: Vec<ChordMark>,
    },
    AddTrack {
        name: String,
        #[serde(default = "none_instr")]
        instrument: Instrument,
        #[serde(default)]
        color: Option<[u8; 3]>,
    },
    RemoveTrack {
        track: TrackRef,
    },
    UpdateTrack {
        track: TrackRef,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        instrument: Option<Instrument>,
        #[serde(default)]
        color: Option<[u8; 3]>,
        /// Game layer for layered stem export ("base", "melody", "percussion"; "" = base).
        #[serde(default)]
        layer: Option<String>,
    },
    SetMixer {
        track: TrackRef,
        mixer: MixerPatch,
    },
    SetMaster {
        master: MasterPatch,
    },
    /// Put a VST3 effect in a track's insert chain (`track`) or a bus's (`bus`), at `index` (default: the end).
    AddInsert {
        #[serde(default)]
        track: Option<TrackRef>,
        #[serde(default)]
        bus: Option<Bus>,
        path: String,
        #[serde(default)]
        class: Option<String>,
        #[serde(default)]
        params: std::collections::BTreeMap<String, Value>,
        #[serde(default)]
        preset: Option<String>,
        #[serde(default)]
        preset_name: String,
        #[serde(default)]
        bypass: bool,
        #[serde(default)]
        position: Option<InsertPosition>,
        #[serde(default)]
        index: Option<usize>,
    },
    /// Change an insert. `params` merge into the existing ones (a null value removes that one); `clear_params`
    /// drops the existing ones first.
    UpdateInsert {
        insert: u64,
        #[serde(default)]
        params: Option<std::collections::BTreeMap<String, Value>>,
        #[serde(default)]
        clear_params: bool,
        #[serde(default, deserialize_with = "double_option")]
        preset: Option<Option<String>>,
        #[serde(default)]
        preset_name: Option<String>,
        #[serde(default)]
        bypass: Option<bool>,
        #[serde(default)]
        position: Option<InsertPosition>,
    },
    RemoveInsert {
        insert: u64,
    },
    /// Reorder an insert within its chain.
    MoveInsert {
        insert: u64,
        index: usize,
    },
    AddNotes {
        track: TrackRef,
        notes: Vec<NoteSpec>,
    },
    RemoveNotes {
        track: TrackRef,
        #[serde(default)]
        from_bar: Option<u32>,
        #[serde(default)]
        to_bar: Option<u32>,
        #[serde(default)]
        pitch: Option<PitchSpec>,
    },
    /// Delete exact notes (matched by start beat within 0.001 and pitch). Batch with add_notes to move/resize.
    DeleteNotes {
        track: TrackRef,
        notes: Vec<NoteKey>,
    },
    /// Snap note starts (and optionally ends) toward a grid in beats (0.25 = 16ths). strength 1 = hard snap.
    Quantize {
        track: TrackRef,
        grid: f64,
        #[serde(default = "d_one")]
        strength: f64,
        #[serde(default)]
        ends: bool,
        #[serde(default)]
        from_bar: Option<u32>,
        #[serde(default)]
        to_bar: Option<u32>,
    },
    /// Deterministic humanization: timing jitter in ms (uniform +-), velocity jitter (+-), length jitter (fraction).
    Humanize {
        track: TrackRef,
        #[serde(default)]
        timing_ms: f64,
        #[serde(default)]
        velocity: u8,
        #[serde(default)]
        length: f64,
        #[serde(default)]
        seed: u64,
        #[serde(default)]
        from_bar: Option<u32>,
        #[serde(default)]
        to_bar: Option<u32>,
    },
    TransposeNotes {
        track: TrackRef,
        semitones: i32,
        #[serde(default)]
        from_bar: Option<u32>,
        #[serde(default)]
        to_bar: Option<u32>,
    },
    ScaleVelocity {
        track: TrackRef,
        factor: f64,
        #[serde(default)]
        from_bar: Option<u32>,
        #[serde(default)]
        to_bar: Option<u32>,
    },
    AddCc {
        track: TrackRef,
        cc: u8,
        points: Vec<CcSpec>,
    },
    ClearCc {
        track: TrackRef,
        #[serde(default)]
        cc: Option<u8>,
        #[serde(default)]
        from_bar: Option<u32>,
        #[serde(default)]
        to_bar: Option<u32>,
    },
    AddBend {
        track: TrackRef,
        points: Vec<BendSpec>,
    },
    /// Copy `bars` bars starting at `from_bar` to `to_bar` (one track, or every track when omitted).
    CopyBars {
        #[serde(default)]
        track: Option<TrackRef>,
        from_bar: u32,
        bars: u32,
        to_bar: u32,
    },
    /// Delete everything (notes, CCs, bends) in a bar range (one track, or every track when omitted).
    ClearBars {
        #[serde(default)]
        track: Option<TrackRef>,
        from_bar: u32,
        bars: u32,
    },
}

impl Command {
    /// One-line human-readable summary for the activity log.
    pub fn describe(&self) -> String {
        match self {
            Command::SetProject { .. } => "set project properties".into(),
            Command::SetTempo { points } => format!("set tempo map ({} points)", points.len()),
            Command::SetSections { sections } => format!("set {} sections", sections.len()),
            Command::SetHarmony { chords } => format!("set chord track ({} chords)", chords.len()),
            Command::AddTrack { name, instrument, .. } => format!("add track '{name}' ({})", instr_kind(instrument)),
            Command::RemoveTrack { track } => format!("remove track {track}"),
            Command::UpdateTrack { track, name, instrument, color, layer } => {
                let mut what = vec![];
                if let Some(n) = name {
                    what.push(format!("rename to '{n}'"));
                }
                if let Some(i) = instrument {
                    what.push(format!("instrument ({})", instr_kind(i)));
                }
                if color.is_some() {
                    what.push("color".into());
                }
                if let Some(l) = layer {
                    what.push(format!("layer '{l}'"));
                }
                format!("update track {track}: {}", what.join(", "))
            }
            Command::SetMixer { track, mixer } => format!("mixer {track}: {}", patch_keys(mixer)),
            Command::SetMaster { .. } => "set master".into(),
            Command::AddInsert { track, bus, path, .. } => {
                let plugin = plugin_name(path);
                let chain = match (track, bus) {
                    (Some(t), _) => format!("track {t}"),
                    (None, Some(Bus::Reverb)) => "the reverb return".into(),
                    (None, _) => "the master".into(),
                };
                format!("add insert {plugin} to {chain}")
            }
            Command::UpdateInsert { insert, params, clear_params, preset, preset_name, bypass, position } => {
                let mut what = vec![];
                if let Some(ps) = params {
                    what.push(format!("{} params", ps.len()));
                }
                if *clear_params {
                    what.push("clear params".into());
                }
                if preset.is_some() || preset_name.is_some() {
                    what.push("preset".into());
                }
                if let Some(b) = bypass {
                    what.push(if *b { "bypass".into() } else { "un-bypass".into() });
                }
                if let Some(p) = position {
                    what.push(format!("{p:?}").to_lowercase());
                }
                format!("update insert #{insert}: {}", what.join(", "))
            }
            Command::RemoveInsert { insert } => format!("remove insert #{insert}"),
            Command::MoveInsert { insert, index } => format!("move insert #{insert} to slot {}", index + 1),
            Command::AddNotes { track, notes } => format!("add {} notes to {track}", notes.len()),
            Command::RemoveNotes { track, from_bar, to_bar, .. } => {
                format!("remove notes from {track} {}", range_str(*from_bar, *to_bar))
            }
            Command::DeleteNotes { track, notes } => format!("delete {} notes from {track}", notes.len()),
            Command::Quantize { track, grid, strength, .. } => format!("quantize {track} to {grid} beats ({:.0}%)", strength * 100.0),
            Command::Humanize { track, timing_ms, velocity, .. } => format!("humanize {track} (+-{timing_ms} ms, +-{velocity} vel)"),
            Command::TransposeNotes { track, semitones, .. } => format!("transpose {track} by {semitones}"),
            Command::ScaleVelocity { track, factor, .. } => format!("scale velocity of {track} x{factor}"),
            Command::AddCc { track, cc, points } => format!("add {} CC{cc} points to {track}", points.len()),
            Command::ClearCc { track, .. } => format!("clear CCs on {track}"),
            Command::AddBend { track, points } => format!("add {} pitch bends to {track}", points.len()),
            Command::CopyBars { from_bar, bars, to_bar, track } => format!(
                "copy bars {from_bar}-{} to bar {to_bar}{}",
                from_bar + bars - 1,
                track.as_ref().map(|t| format!(" ({t})")).unwrap_or_default()
            ),
            Command::ClearBars { from_bar, bars, track } => format!(
                "clear bars {from_bar}-{}{}",
                from_bar + bars - 1,
                track.as_ref().map(|t| format!(" ({t})")).unwrap_or_default()
            ),
        }
    }
}

fn instr_kind(i: &Instrument) -> &'static str {
    match i {
        Instrument::Sfz { .. } => "sfz",
        Instrument::Vst3 { .. } => "vst3",
        Instrument::Audio { .. } => "audio",
        Instrument::None => "no instrument",
    }
}

fn patch_keys(m: &MixerPatch) -> String {
    let v = serde_json::to_value(m).unwrap_or(Value::Null);
    let mut keys = vec![];
    if let Value::Object(o) = v {
        for (k, val) in o {
            if !val.is_null() {
                keys.push(k);
            }
        }
    }
    keys.join(", ")
}

fn range_str(a: Option<u32>, b: Option<u32>) -> String {
    match (a, b) {
        (None, None) => "(all bars)".into(),
        (a, b) => format!("bars {}-{}", a.map(|x| x.to_string()).unwrap_or("1".into()), b.map(|x| x.to_string()).unwrap_or("end".into())),
    }
}

fn track_mut<'a>(p: &'a mut Project, r: &TrackRef) -> Result<&'a mut Track> {
    let i = p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?;
    Ok(&mut p.tracks[i])
}

fn beat_range(tm: &TimeMap, from_bar: Option<u32>, to_bar: Option<u32>) -> (f64, f64) {
    let a = from_bar.map(|b| tm.bar_beat(b)).unwrap_or(f64::NEG_INFINITY);
    let b = to_bar.map(|b| tm.bar_beat(b + 1)).unwrap_or(f64::INFINITY);
    (a, b)
}

fn spec_beat(tm: &TimeMap, bar: Option<u32>, beat: f64) -> f64 {
    match bar {
        Some(b) => tm.bar_beat(b) + beat,
        None => beat,
    }
}

pub fn apply(p: &mut Project, cmd: &Command) -> Result<Value> {
    let tm = TimeMap::new(p);
    match cmd {
        Command::SetProject { name, key, bars, looped, time_signature, bpm, notes } => {
            if let Some(v) = name {
                p.name = v.clone();
            }
            if let Some(v) = key {
                p.key = v.clone();
            }
            if let Some(v) = bars {
                if *v == 0 {
                    bail!("bars must be > 0");
                }
                p.bars = *v;
            }
            if let Some(v) = looped {
                p.looped = *v;
            }
            if let Some(v) = time_signature {
                p.time_signature = *v;
            }
            if let Some(v) = bpm {
                p.tempo = vec![TempoPoint { bar: 1, bpm: *v }];
            }
            if let Some(v) = notes {
                p.notes = v.clone();
            }
            Ok(json!({"ok": true}))
        }
        Command::SetTempo { points } => {
            if points.is_empty() || points.iter().any(|t| t.bpm <= 0.0) {
                bail!("tempo points need bpm > 0");
            }
            let mut pts = points.clone();
            pts.sort_by_key(|t| t.bar);
            p.tempo = pts;
            Ok(json!({"ok": true}))
        }
        Command::SetSections { sections } => {
            p.sections = sections.clone();
            p.sections.sort_by_key(|s| s.start_bar);
            Ok(json!({"ok": true}))
        }
        Command::SetHarmony { chords } => {
            let (num, den) = p.time_signature;
            let bar_beats = num as f64 * 4.0 / den.max(1) as f64;
            for c in chords {
                if c.bar < 1 || c.bar > p.bars {
                    bail!("chord '{}' at bar {}: the song has bars 1-{}", c.chord, c.bar, p.bars);
                }
                if !(0.0..bar_beats).contains(&c.beat) {
                    bail!("chord '{}' at bar {}: beat {} is outside the bar (0-{bar_beats})", c.chord, c.bar, c.beat);
                }
                if c.chord.trim().is_empty() {
                    bail!("empty chord symbol at bar {}", c.bar);
                }
            }
            let mut v = chords.clone();
            for c in &mut v {
                c.chord = c.chord.trim().to_string();
            }
            v.sort_by(|a, b| (a.bar, a.beat).partial_cmp(&(b.bar, b.beat)).unwrap_or(std::cmp::Ordering::Equal));
            if let Some(w) = v.windows(2).find(|w| w[0].bar == w[1].bar && (w[0].beat - w[1].beat).abs() < 1e-6) {
                bail!("two chords at bar {} beat {}: '{}' and '{}'", w[0].bar, w[0].beat, w[0].chord, w[1].chord);
            }
            p.harmony = v;
            Ok(json!({"ok": true, "chords": p.harmony.len()}))
        }
        Command::AddTrack { name, instrument, color } => {
            if p.tracks.iter().any(|t| t.name == *name) {
                bail!("a track named '{name}' already exists");
            }
            let id = p.alloc_id();
            let color = color.unwrap_or(PALETTE[(id as usize - 1) % PALETTE.len()]);
            p.tracks.push(Track {
                id,
                name: name.clone(),
                color,
                instrument: instrument.clone(),
                notes: vec![],
                ccs: vec![],
                bends: vec![],
                mixer: Mixer::default(),
                meta: Default::default(),
                layer: String::new(),
            });
            Ok(json!({"track_id": id}))
        }
        Command::RemoveTrack { track } => {
            let i = p.track_index(track).ok_or_else(|| anyhow!("no track {track}"))?;
            let t = p.tracks.remove(i);
            for o in p.tracks.iter_mut() {
                if o.mixer.duck.map(|d| d.source_track) == Some(t.id) {
                    o.mixer.duck = None;
                }
            }
            Ok(json!({"removed": t.name}))
        }
        Command::UpdateTrack { track, name, instrument, color, layer } => {
            if let Some(n) = name
                && p.tracks.iter().any(|t| t.name == *n) {
                    bail!("a track named '{n}' already exists");
                }
            let t = track_mut(p, track)?;
            if let Some(n) = name {
                t.name = n.clone();
            }
            if let Some(i) = instrument {
                t.instrument = i.clone();
            }
            if let Some(c) = color {
                t.color = *c;
            }
            if let Some(l) = layer {
                t.layer = l.trim().to_lowercase();
            }
            Ok(json!({"ok": true}))
        }
        Command::SetMixer { track, mixer: m } => {
            let duck = match &m.duck {
                Some(Some(d)) => {
                    let src = p.track_index(&d.source_track).ok_or_else(|| anyhow!("no duck source {}", d.source_track))?;
                    Some(Some(Duck { source_track: p.tracks[src].id, depth_db: d.depth_db, release_ms: d.release_ms }))
                }
                Some(None) => Some(None),
                None => None,
            };
            for s in [m.hpf_slope, m.lpf_slope].into_iter().flatten() {
                if !FILTER_SLOPES.contains(&s) {
                    bail!("a filter slope of {s} dB/octave: it can be 12, 24, 36 or 48");
                }
            }
            let t = track_mut(p, track)?;
            let x = &mut t.mixer;
            if let Some(v) = m.gain_db {
                x.gain_db = v;
            }
            if let Some(v) = m.pan {
                x.pan = v.clamp(-1.0, 1.0);
            }
            if let Some(v) = m.width {
                x.width = v.clamp(0.0, 2.0);
            }
            if let Some(v) = m.mute {
                x.mute = v;
            }
            if let Some(v) = m.solo {
                x.solo = v;
            }
            if let Some(v) = m.hpf_hz {
                x.hpf_hz = v.max(0.0);
            }
            if let Some(v) = m.lpf_hz {
                x.lpf_hz = v.max(0.0);
            }
            if let Some(v) = m.hpf_slope {
                x.hpf_slope = v;
            }
            if let Some(v) = m.lpf_slope {
                x.lpf_slope = v;
            }
            if let Some(v) = m.mono_below_hz {
                x.mono_below_hz = v.clamp(0.0, 1000.0);
            }
            if let Some(v) = &m.eq {
                x.eq = v.clone();
            }
            if let Some(v) = m.drive_db {
                x.drive_db = v.max(0.0);
            }
            if let Some(v) = m.amp {
                x.amp = v;
            }
            if let Some(v) = m.chorus {
                x.chorus = v;
            }
            if let Some(v) = m.bitcrush_bits {
                x.bitcrush_bits = if v <= 0.0 { 0.0 } else { v.clamp(1.0, 24.0) };
            }
            if let Some(v) = m.clip_db {
                x.clip_db = v;
            }
            if let Some(v) = m.post_gain_db {
                x.post_gain_db = v;
            }
            if let Some(v) = m.gate {
                x.gate = v;
            }
            if let Some(v) = m.comp {
                x.comp = v;
            }
            if let Some(v) = m.delay {
                x.delay = v;
            }
            if let Some(v) = duck {
                x.duck = v;
            }
            if let Some(v) = m.reverb_send {
                x.reverb_send = v.clamp(0.0, 1.0);
            }
            Ok(json!({"mixer": t.mixer}))
        }
        Command::SetMaster { master: m } => {
            let x = &mut p.master;
            if let Some(v) = m.glue {
                x.glue = v;
            }
            if let Some(v) = &m.eq {
                x.eq = v.clone();
            }
            if let Some(v) = m.target_lufs {
                x.target_lufs = v;
            }
            if let Some(v) = m.ceiling_dbtp {
                x.ceiling_dbtp = v.min(0.0);
            }
            if let Some(v) = m.reverb {
                x.reverb = v;
            }
            if let Some(v) = m.edge_fade_ms {
                x.edge_fade_ms = v.max(0.0);
            }
            if let Some(v) = m.tail_s {
                x.tail_s = v.clamp(0.0, 30.0);
            }
            Ok(json!({"master": p.master}))
        }
        Command::AddInsert { track, bus, path, class, params, preset, preset_name, bypass, position, index } => {
            if path.trim().is_empty() {
                bail!("add_insert needs the plugin's .vst3 path");
            }
            let chain = match (track, bus) {
                (Some(_), Some(_)) => bail!("give either track or bus, not both"),
                (Some(r), None) => Chain::Track(p.tracks[p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?].id),
                (None, Some(Bus::Master)) => Chain::Master,
                (None, Some(Bus::Reverb)) => Chain::Reverb,
                (None, None) => bail!("add_insert needs a track, or bus \"master\" or \"reverb\""),
            };
            if position.is_some() && !matches!(chain, Chain::Track(_)) {
                bail!("position applies to track inserts only (a bus chain has one place)");
            }
            let id = p.alloc_id();
            let ins = Insert {
                id,
                path: path.trim().to_string(),
                class: class.clone(),
                params: params.clone(),
                preset: preset.clone(),
                preset_name: preset_name.clone(),
                bypass: *bypass,
                position: position.unwrap_or_default(),
            };
            let v = p.chain_mut(chain).ok_or_else(|| anyhow!("no chain {chain:?}"))?;
            let at = index.unwrap_or(v.len()).min(v.len());
            v.insert(at, ins);
            Ok(json!({"insert_id": id, "chain": chain, "index": at}))
        }
        Command::UpdateInsert { insert, params, clear_params, preset, preset_name, bypass, position } => {
            let (chain, _, _) = p.insert(*insert).ok_or_else(|| anyhow!("no insert #{insert}"))?;
            if position.is_some() && !matches!(chain, Chain::Track(_)) {
                bail!("position applies to track inserts only (a bus chain has one place)");
            }
            let ins = p.insert_mut(*insert).ok_or_else(|| anyhow!("no insert #{insert}"))?;
            if *clear_params {
                ins.params.clear();
            }
            for (k, v) in params.iter().flatten() {
                if v.is_null() {
                    ins.params.remove(k);
                } else {
                    ins.params.insert(k.clone(), v.clone());
                }
            }
            if let Some(v) = preset {
                ins.preset = v.clone();
                if v.is_none() && preset_name.is_none() {
                    ins.preset_name.clear();
                }
            }
            if let Some(v) = preset_name {
                ins.preset_name = v.clone();
            }
            if let Some(v) = bypass {
                ins.bypass = *v;
            }
            if let Some(v) = position {
                ins.position = *v;
            }
            Ok(json!({"insert": ins}))
        }
        Command::RemoveInsert { insert } => {
            let (chain, at, _) = p.insert(*insert).ok_or_else(|| anyhow!("no insert #{insert}"))?;
            let v = p.chain_mut(chain).ok_or_else(|| anyhow!("no chain {chain:?}"))?;
            let gone = v.remove(at);
            Ok(json!({"removed": gone.plugin_name(), "chain": chain}))
        }
        Command::MoveInsert { insert, index } => {
            let (chain, at, _) = p.insert(*insert).ok_or_else(|| anyhow!("no insert #{insert}"))?;
            let v = p.chain_mut(chain).ok_or_else(|| anyhow!("no chain {chain:?}"))?;
            let ins = v.remove(at);
            let to = (*index).min(v.len());
            v.insert(to, ins);
            Ok(json!({"chain": chain, "index": to}))
        }
        Command::AddNotes { track, notes } => {
            let mut new = Vec::with_capacity(notes.len());
            for n in notes {
                if n.dur <= 0.0 {
                    bail!("note duration must be > 0");
                }
                let pitch = n.pitch.resolve()?;
                if pitch > 127 || n.vel == 0 || n.vel > 127 {
                    bail!("pitch 0-127 and vel 1-127");
                }
                new.push(Note { start: spec_beat(&tm, n.bar, n.beat), dur: n.dur, pitch, vel: n.vel });
            }
            let t = track_mut(p, track)?;
            t.notes.extend(new);
            t.notes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap().then(a.pitch.cmp(&b.pitch)));
            Ok(json!({"notes_in_track": t.notes.len()}))
        }
        Command::RemoveNotes { track, from_bar, to_bar, pitch } => {
            let (a, b) = beat_range(&tm, *from_bar, *to_bar);
            let pf = match pitch {
                Some(ps) => Some(ps.resolve()?),
                None => None,
            };
            let t = track_mut(p, track)?;
            let before = t.notes.len();
            t.notes.retain(|n| !(n.start >= a - 1e-9 && n.start < b - 1e-9 && pf.is_none_or(|x| x == n.pitch)));
            Ok(json!({"removed": before - t.notes.len()}))
        }
        Command::DeleteNotes { track, notes } => {
            let mut keys = Vec::with_capacity(notes.len());
            for k in notes {
                keys.push((spec_beat(&tm, k.bar, k.beat), k.pitch.resolve()?));
            }
            let t = track_mut(p, track)?;
            let mut missing = vec![];
            for (start, pitch) in keys {
                match t.notes.iter().position(|n| n.pitch == pitch && (n.start - start).abs() < 1e-3) {
                    Some(i) => {
                        t.notes.remove(i);
                    }
                    None => missing.push(format!("{}@{start}", crate::pitch::name(pitch))),
                }
            }
            if !missing.is_empty() {
                bail!("notes not found: {}", missing.join(", "));
            }
            Ok(json!({"notes_in_track": t.notes.len()}))
        }
        Command::Quantize { track, grid, strength, ends, from_bar, to_bar } => {
            if *grid <= 0.0 {
                bail!("grid must be > 0 (beats)");
            }
            let (a, b) = beat_range(&tm, *from_bar, *to_bar);
            let s = strength.clamp(0.0, 1.0);
            let t = track_mut(p, track)?;
            let mut k = 0;
            for n in t.notes.iter_mut().filter(|n| n.start >= a - 1e-9 && n.start < b - 1e-9) {
                let end = n.start + n.dur;
                n.start += ((n.start / grid).round() * grid - n.start) * s;
                let new_end = if *ends { end + ((end / grid).round() * grid - end) * s } else { n.start + n.dur };
                n.dur = (new_end - n.start).max(grid * 0.25);
                k += 1;
            }
            t.notes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap().then(a.pitch.cmp(&b.pitch)));
            Ok(json!({"quantized": k}))
        }
        Command::Humanize { track, timing_ms, velocity, length, seed, from_bar, to_bar } => {
            let (a, b) = beat_range(&tm, *from_bar, *to_bar);
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03);
            let t = track_mut(p, track)?;
            let mut k = 0;
            for n in t.notes.iter_mut().filter(|n| n.start >= a - 1e-9 && n.start < b - 1e-9) {
                let dt = rng.sym() * timing_ms / 1000.0 / tm.spb_at(n.start);
                n.start = (n.start + dt).max(0.0);
                n.dur = (n.dur * (1.0 + rng.sym() * length)).max(0.01);
                let dv = (rng.sym() * *velocity as f64).round();
                n.vel = (n.vel as f64 + dv).clamp(1.0, 127.0) as u8;
                k += 1;
            }
            t.notes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap().then(a.pitch.cmp(&b.pitch)));
            Ok(json!({"humanized": k}))
        }
        Command::TransposeNotes { track, semitones, from_bar, to_bar } => {
            let (a, b) = beat_range(&tm, *from_bar, *to_bar);
            let t = track_mut(p, track)?;
            let mut n_changed = 0;
            for n in t.notes.iter_mut().filter(|n| n.start >= a - 1e-9 && n.start < b - 1e-9) {
                let q = n.pitch as i32 + semitones;
                if !(0..=127).contains(&q) {
                    bail!("transpose would leave MIDI range");
                }
                n.pitch = q as u8;
                n_changed += 1;
            }
            Ok(json!({"transposed": n_changed}))
        }
        Command::ScaleVelocity { track, factor, from_bar, to_bar } => {
            let (a, b) = beat_range(&tm, *from_bar, *to_bar);
            let t = track_mut(p, track)?;
            let mut k = 0;
            for n in t.notes.iter_mut().filter(|n| n.start >= a - 1e-9 && n.start < b - 1e-9) {
                n.vel = ((n.vel as f64 * factor).round()).clamp(1.0, 127.0) as u8;
                k += 1;
            }
            Ok(json!({"scaled": k}))
        }
        Command::AddCc { track, cc, points } => {
            let pts: Vec<CcPoint> =
                points.iter().map(|c| CcPoint { beat: spec_beat(&tm, c.bar, c.beat), cc: *cc, value: c.value.min(127) }).collect();
            let t = track_mut(p, track)?;
            t.ccs.extend(pts);
            t.ccs.sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap());
            Ok(json!({"cc_points_in_track": t.ccs.len()}))
        }
        Command::ClearCc { track, cc, from_bar, to_bar } => {
            let (a, b) = beat_range(&tm, *from_bar, *to_bar);
            let t = track_mut(p, track)?;
            let before = t.ccs.len();
            t.ccs.retain(|c| !(c.beat >= a - 1e-9 && c.beat < b - 1e-9 && cc.is_none_or(|x| x == c.cc)));
            Ok(json!({"removed": before - t.ccs.len()}))
        }
        Command::AddBend { track, points } => {
            let pts: Vec<BendPoint> =
                points.iter().map(|c| BendPoint { beat: spec_beat(&tm, c.bar, c.beat), value: c.value.clamp(-8192, 8191) }).collect();
            let t = track_mut(p, track)?;
            t.bends.extend(pts);
            t.bends.sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap());
            Ok(json!({"bends_in_track": t.bends.len()}))
        }
        Command::CopyBars { track, from_bar, bars, to_bar } => {
            let a = tm.bar_beat(*from_bar);
            let b = tm.bar_beat(from_bar + bars);
            let off = tm.bar_beat(*to_bar) - a;
            let idx: Vec<usize> = match track {
                Some(r) => vec![p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?],
                None => (0..p.tracks.len()).collect(),
            };
            let mut copied = 0;
            for i in idx {
                let t = &mut p.tracks[i];
                let notes: Vec<Note> = t
                    .notes
                    .iter()
                    .filter(|n| n.start >= a - 1e-9 && n.start < b - 1e-9)
                    .map(|n| Note { start: n.start + off, ..*n })
                    .collect();
                let ccs: Vec<CcPoint> =
                    t.ccs.iter().filter(|c| c.beat >= a - 1e-9 && c.beat < b - 1e-9).map(|c| CcPoint { beat: c.beat + off, ..*c }).collect();
                copied += notes.len();
                t.notes.extend(notes);
                t.ccs.extend(ccs);
                t.notes.sort_by(|x, y| x.start.partial_cmp(&y.start).unwrap());
                t.ccs.sort_by(|x, y| x.beat.partial_cmp(&y.beat).unwrap());
            }
            Ok(json!({"notes_copied": copied}))
        }
        Command::ClearBars { track, from_bar, bars } => {
            let a = tm.bar_beat(*from_bar);
            let b = tm.bar_beat(from_bar + bars);
            let idx: Vec<usize> = match track {
                Some(r) => vec![p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?],
                None => (0..p.tracks.len()).collect(),
            };
            let mut removed = 0;
            for i in idx {
                let t = &mut p.tracks[i];
                let before = t.notes.len();
                t.notes.retain(|n| !(n.start >= a - 1e-9 && n.start < b - 1e-9));
                t.ccs.retain(|c| !(c.beat >= a - 1e-9 && c.beat < b - 1e-9));
                t.bends.retain(|c| !(c.beat >= a - 1e-9 && c.beat < b - 1e-9));
                removed += before - t.notes.len();
            }
            Ok(json!({"notes_removed": removed}))
        }
    }
}

/// Small deterministic generator (splitmix64) so humanization is reproducible from a seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [-1, 1).
    fn sym(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: &mut Project, v: Value) -> Result<Value> {
        apply(p, &serde_json::from_value(v)?)
    }

    #[test]
    fn insert_chains() -> Result<()> {
        let mut p = Project::new("t", 120.0, 4);
        run(&mut p, json!({"op": "add_track", "name": "pad"}))?;
        let sm = "C:/Program Files/Common Files/VST3/ValhallaSupermassive.vst3";
        let a = run(&mut p, json!({"op": "add_insert", "track": "pad", "path": sm, "params": {"Mix": 0.3}}))?["insert_id"].as_u64().unwrap();
        let b = run(&mut p, json!({"op": "add_insert", "track": "pad", "path": "x/iZotope Vinyl.vst3", "position": "pre", "index": 0}))?
            ["insert_id"]
            .as_u64()
            .unwrap();
        let r = run(&mut p, json!({"op": "add_insert", "bus": "reverb", "path": sm}))?["insert_id"].as_u64().unwrap();
        let m = run(&mut p, json!({"op": "add_insert", "bus": "master", "path": "x/iZotope Vinyl.vst3"}))?["insert_id"].as_u64().unwrap();
        // ids are shared with tracks and never collide
        let t2 = run(&mut p, json!({"op": "add_track", "name": "lead"}))?["track_id"].as_u64().unwrap();
        let mut ids = vec![1, a, b, r, m, t2];
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 6);

        let pad = &p.tracks[0].mixer.inserts;
        assert_eq!(pad.iter().map(|i| i.id).collect::<Vec<_>>(), [b, a]);
        assert_eq!(pad[0].position, InsertPosition::Pre);
        assert_eq!(pad[1].plugin_name(), "ValhallaSupermassive");
        assert_eq!(p.insert(r).map(|(c, at, _)| (c, at)), Some((Chain::Reverb, 0)));
        assert_eq!(p.insert(m).map(|(c, _, _)| c), Some(Chain::Master));
        assert!(run(&mut p, json!({"op": "add_insert", "bus": "master", "path": sm, "position": "pre"})).is_err());
        assert!(run(&mut p, json!({"op": "add_insert", "path": sm})).is_err());

        run(&mut p, json!({"op": "update_insert", "insert": a, "params": {"Mix": null, "Feedback": 0.5}, "bypass": true, "preset": "plugin-states/x.vstpreset", "preset_name": "Sagittarius"}))?;
        let ia = p.insert(a).unwrap().2;
        assert!(ia.bypass && ia.params.len() == 1 && ia.params["Feedback"] == json!(0.5) && ia.preset_name == "Sagittarius");
        run(&mut p, json!({"op": "update_insert", "insert": a, "preset": null}))?;
        let ia = p.insert(a).unwrap().2;
        assert!(ia.preset.is_none() && ia.preset_name.is_empty());

        run(&mut p, json!({"op": "move_insert", "insert": a, "index": 0}))?;
        assert_eq!(p.tracks[0].mixer.inserts[0].id, a);

        // round trip through the file format; a project without inserts doesn't mention them
        let back: Project = serde_json::from_str(&serde_json::to_string(&p)?)?;
        assert_eq!(serde_json::to_value(&back)?, serde_json::to_value(&p)?);
        assert!(!serde_json::to_string(&Project::new("e", 90.0, 2))?.contains("inserts"));

        run(&mut p, json!({"op": "remove_insert", "insert": r}))?;
        assert!(p.master.reverb_inserts.is_empty() && p.insert(r).is_none());
        run(&mut p, json!({"op": "remove_track", "track": "pad"}))?;
        assert!(p.insert(a).is_none() && p.insert(b).is_none());
        // a removed track's id isn't handed out again
        let t3 = run(&mut p, json!({"op": "add_track", "name": "bass"}))?["track_id"].as_u64().unwrap();
        assert!(t3 > t2);
        Ok(())
    }

    #[test]
    fn filter_slopes() -> Result<()> {
        let mut p = Project::new("t", 120.0, 4);
        run(&mut p, json!({"op": "add_track", "name": "glass"}))?;
        assert_eq!((p.tracks[0].mixer.hpf_slope, p.tracks[0].mixer.lpf_slope), (12, 12));
        run(&mut p, json!({"op": "set_mixer", "track": "glass", "mixer": {"hpf_hz": 190, "hpf_slope": 48}}))?;
        assert_eq!((p.tracks[0].mixer.hpf_hz, p.tracks[0].mixer.hpf_slope, p.tracks[0].mixer.lpf_slope), (190.0, 48, 12));
        // a slope it doesn't have changes nothing
        let e = run(&mut p, json!({"op": "set_mixer", "track": "glass", "mixer": {"gain_db": -3, "lpf_slope": 18}})).unwrap_err().to_string();
        assert!(e.contains("12, 24, 36 or 48"), "{e}");
        assert_eq!(p.tracks[0].mixer.gain_db, 0.0);
        // projects saved before slopes existed open with 12 dB
        let mut v = serde_json::to_value(&p)?;
        v["tracks"][0]["mixer"].as_object_mut().unwrap().retain(|k, _| !k.ends_with("_slope"));
        let old: Project = serde_json::from_value(v)?;
        assert_eq!((old.tracks[0].mixer.hpf_slope, old.tracks[0].mixer.lpf_slope), (12, 12));
        assert_eq!(old.tracks[0].mixer.mono_below_hz, 0.0);
        run(&mut p, json!({"op": "set_mixer", "track": "glass", "mixer": {"mono_below_hz": 120}}))?;
        assert_eq!(p.tracks[0].mixer.mono_below_hz, 120.0);
        Ok(())
    }
}
