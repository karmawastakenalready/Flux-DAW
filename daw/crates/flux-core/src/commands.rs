//! Every edit to a project is a `Command`. Agents send them as JSON (`{"op": "add_notes", ...}`);
//! the GUI builds the same values. `apply` mutates the project and returns a small JSON result.

use crate::automation::{Kind, Lane, Point, Shape, Target};
use crate::clip::{self, Clip};
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

/// An automation point: `bar` + `beat` within it, or absolute `beat`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoPointSpec {
    #[serde(default)]
    pub bar: Option<u32>,
    #[serde(default)]
    pub beat: f64,
    pub value: f64,
    #[serde(default)]
    pub shape: Shape,
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
    /// Replaces the dynamic EQ bands.
    pub dyn_eq: Option<Vec<DynBand>>,
    #[serde(default, deserialize_with = "double_option")]
    pub deess: Option<Option<DeEss>>,
    #[serde(default, deserialize_with = "double_option")]
    pub reverb: Option<Option<ReverbFx>>,
    /// The bus track this one's output goes to; null = the master.
    #[serde(default, deserialize_with = "double_option")]
    pub output: Option<Option<TrackRef>>,
    /// Replaces the track's sends.
    pub sends: Option<Vec<SendSpec>>,
}

/// A send as commands write it: the receiving track by name or id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendSpec {
    pub to: TrackRef,
    pub level: f64,
}

/// A track sending to a new bus (`add_bus`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusSend {
    pub track: TrackRef,
    pub level: f64,
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
    /// The whole limiter: settings left out take their defaults.
    pub limiter: Option<Limiter>,
    #[serde(default, deserialize_with = "double_option")]
    pub multiband: Option<Option<Multiband>>,
    pub mono_below_hz: Option<f64>,
    pub width: Option<f64>,
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
    /// Add a bus: a track without an instrument that `tracks` route their output to (a group, for shared EQ,
    /// compression and level) and that `sends` send to (an aux return: give it a reverb or a delay at mix 1).
    /// `mixer` sets its strip.
    AddBus {
        name: String,
        #[serde(default)]
        tracks: Vec<TrackRef>,
        #[serde(default)]
        sends: Vec<BusSend>,
        #[serde(default)]
        mixer: Option<MixerPatch>,
        #[serde(default)]
        color: Option<[u8; 3]>,
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
    /// Write automation: a track's volume, pan or reverb send, or a VST3 parameter of one of its inserts
    /// (`insert`) or of its instrument (no `insert`); with `bus`, a parameter of a master or reverb-return insert.
    /// The points replace the lane's from the first given beat to the last, or with `replace` the whole lane (no
    /// points then delete it).
    AddAutomation {
        #[serde(default)]
        track: Option<TrackRef>,
        #[serde(default)]
        bus: Option<Bus>,
        target: Kind,
        #[serde(default)]
        insert: Option<u64>,
        #[serde(default)]
        param: Option<String>,
        #[serde(default)]
        normalized: bool,
        points: Vec<AutoPointSpec>,
        #[serde(default)]
        replace: bool,
    },
    /// Remove automation points in a bar range, or whole lanes without one (a track's, or with `bus` the
    /// master's); `target`, `insert` and `param` narrow it to some lanes.
    ClearAutomation {
        #[serde(default)]
        track: Option<TrackRef>,
        #[serde(default)]
        bus: Option<Bus>,
        #[serde(default)]
        target: Option<Kind>,
        #[serde(default)]
        insert: Option<u64>,
        #[serde(default)]
        param: Option<String>,
        #[serde(default)]
        from_bar: Option<u32>,
        #[serde(default)]
        to_bar: Option<u32>,
    },
    /// Copy `bars` bars starting at `from_bar` to `to_bar`: notes, CCs and bends join what is there, automation
    /// replaces it (one track, or every track and the master when omitted).
    CopyBars {
        #[serde(default)]
        track: Option<TrackRef>,
        from_bar: u32,
        bars: u32,
        to_bar: u32,
    },
    /// Delete everything (notes, CCs, bends, automation points, the audio clips' audio) in a bar range (one track,
    /// or every track and the master when omitted). Clips are cut at the range's edges.
    ClearBars {
        #[serde(default)]
        track: Option<TrackRef>,
        from_bar: u32,
        bars: u32,
    },
    /// Place an audio file on an audio track (a track without an instrument becomes one) at `bar` + `beat`, or
    /// absolute `beat`. It plays from `offset` seconds into the file for `length` seconds (default: the rest of it),
    /// at `gain_db`, with fades in and out (seconds).
    AddClip {
        track: TrackRef,
        path: String,
        #[serde(default)]
        bar: Option<u32>,
        #[serde(default)]
        beat: f64,
        #[serde(default)]
        offset: f64,
        #[serde(default)]
        length: Option<f64>,
        #[serde(default)]
        gain_db: f64,
        #[serde(default)]
        fade_in: f64,
        #[serde(default)]
        fade_out: f64,
    },
    /// Change a clip; what isn't given stays. `bar`/`beat` move it, or with `trim` move only its start (the audio
    /// stays where it is: its front is cut off, or more of the file uncovered); `track` moves it to another audio
    /// track; `length: null` plays to the end of the file.
    UpdateClip {
        clip: u64,
        #[serde(default)]
        track: Option<TrackRef>,
        #[serde(default)]
        bar: Option<u32>,
        #[serde(default)]
        beat: Option<f64>,
        #[serde(default)]
        trim: bool,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        offset: Option<f64>,
        #[serde(default, deserialize_with = "double_option")]
        length: Option<Option<f64>>,
        #[serde(default)]
        gain_db: Option<f64>,
        #[serde(default)]
        fade_in: Option<f64>,
        #[serde(default)]
        fade_out: Option<f64>,
    },
    RemoveClip {
        clip: u64,
    },
    /// Cut a clip in two at `bar` + `beat` (or absolute `beat`): the part after it becomes a new clip.
    SplitClip {
        clip: u64,
        #[serde(default)]
        bar: Option<u32>,
        #[serde(default)]
        beat: f64,
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
            Command::AddBus { name, tracks, sends, .. } => {
                let mut what = vec![];
                if !tracks.is_empty() {
                    what.push(format!("{} routed", tracks.len()));
                }
                if !sends.is_empty() {
                    what.push(format!("{} sending", sends.len()));
                }
                if what.is_empty() { format!("add bus '{name}'") } else { format!("add bus '{name}' ({})", what.join(", ")) }
            }
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
            Command::AddAutomation { track, target, insert, param, points, replace, .. } => {
                let what = match target {
                    Kind::Volume => "volume".into(),
                    Kind::Pan => "pan".into(),
                    Kind::ReverbSend => "reverb send".into(),
                    Kind::Param => Target::Param { insert: *insert, param: param.clone().unwrap_or_default() }.label(),
                };
                let on = track.as_ref().map(|t| format!("{t}")).unwrap_or_else(|| "the master".into());
                match (points.len(), replace) {
                    (0, true) => format!("remove the {what} automation on {on}"),
                    (n, true) => format!("rewrite the {what} automation on {on} ({n} points)"),
                    (n, false) => format!("automate {what} on {on} ({n} points)"),
                }
            }
            Command::ClearAutomation { track, from_bar, to_bar, .. } => format!(
                "clear automation on {} {}",
                track.as_ref().map(|t| format!("{t}")).unwrap_or_else(|| "the master".into()),
                range_str(*from_bar, *to_bar)
            ),
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
            Command::AddClip { track, path, bar, beat, .. } => {
                format!("add clip {} to {track} at {}", Clip::new(0, path.as_str(), 0.0).file_name(), at_str(*bar, *beat))
            }
            Command::UpdateClip { clip, track, bar, beat, trim, path, offset, length, gain_db, fade_in, fade_out } => {
                let mut what = vec![];
                if bar.is_some() || beat.is_some() {
                    let to = at_str(*bar, beat.unwrap_or(0.0));
                    what.push(if *trim { format!("start at {to}") } else { format!("move to {to}") });
                }
                if let Some(t) = track {
                    what.push(format!("to track {t}"));
                }
                if offset.is_some() || length.is_some() {
                    what.push("trim".into());
                }
                if gain_db.is_some() {
                    what.push("gain".into());
                }
                if fade_in.is_some() || fade_out.is_some() {
                    what.push("fades".into());
                }
                if path.is_some() {
                    what.push("file".into());
                }
                format!("update clip #{clip}: {}", what.join(", "))
            }
            Command::RemoveClip { clip } => format!("remove clip #{clip}"),
            Command::SplitClip { clip, bar, beat } => format!("split clip #{clip} at {}", at_str(*bar, *beat)),
        }
    }
}

/// "bar 5", "bar 5 beat 2" or "beat 18".
fn at_str(bar: Option<u32>, beat: f64) -> String {
    match bar {
        Some(b) if beat == 0.0 => format!("bar {b}"),
        Some(b) => format!("bar {b} beat {beat}"),
        None => format!("beat {beat}"),
    }
}

/// An audio instrument with the file it names split off (the file becomes a clip at the top).
fn take_file(i: &Instrument) -> (Instrument, Option<String>) {
    match i {
        Instrument::Audio { path } if !path.trim().is_empty() => (Instrument::Audio { path: String::new() }, Some(path.trim().to_string())),
        Instrument::Audio { .. } => (Instrument::Audio { path: String::new() }, None),
        other => (other.clone(), None),
    }
}

/// The index of a track for clips: an audio track, or one without an instrument (which becomes an audio track).
fn audio_track(p: &mut Project, r: &TrackRef) -> Result<usize> {
    let i = p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?;
    let t = &mut p.tracks[i];
    match t.instrument {
        Instrument::Audio { .. } => {}
        Instrument::None => t.instrument = Instrument::Audio { path: String::new() },
        _ => bail!("track '{}' plays an instrument: clips go on an audio track (one with instrument {{\"kind\": \"audio\"}})", t.name),
    }
    Ok(i)
}

fn sort_clips(t: &mut Track) {
    t.clips.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.id.cmp(&b.id)));
}

/// A clip's values checked, its gain kept in range.
fn checked(mut c: Clip) -> Result<Clip> {
    c.path = c.path.trim().to_string();
    if c.path.is_empty() {
        bail!("a clip needs its audio file's path");
    }
    for (what, v) in [("beat", c.start), ("offset", c.offset), ("gain_db", c.gain_db), ("fade_in", c.fade_in), ("fade_out", c.fade_out)] {
        if !v.is_finite() {
            bail!("a clip's {what} must be a number");
        }
    }
    if c.start < -clip::EPS {
        bail!("a clip at beat {}: the song starts at 0", c.start);
    }
    c.start = c.start.max(0.0);
    if c.offset < 0.0 {
        bail!("a clip can't start before its file does (offset {} s)", c.offset);
    }
    if c.fade_in < 0.0 || c.fade_out < 0.0 {
        bail!("fades are lengths in seconds: 0 or more");
    }
    if let Some(l) = c.length
        && !(l > 0.0 && l.is_finite())
    {
        bail!("a clip's length is in seconds and more than 0 (or null: to the end of its file)");
    }
    c.gain_db = c.gain_db.clamp(clip::MIN_GAIN_DB, clip::MAX_GAIN_DB);
    Ok(c)
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

/// The track `from` may route or send to (`what`): another track, and not one whose own output or sends lead back
/// to `from` (the signal would go round for ever).
fn route_to(p: &Project, from: u64, to: &TrackRef, what: &str) -> Result<u64> {
    let i = p.track_index(to).ok_or_else(|| anyhow!("no track {to} to {what} to"))?;
    let id = p.tracks[i].id;
    if id == from {
        bail!("a track can't {what} to itself");
    }
    if reaches(p, id, from) {
        bail!("{to} already feeds this track (through its output or sends): {what}ing to it would make a loop");
    }
    Ok(id)
}

/// Whether the signal of track `from` reaches track `to` through outputs and sends.
pub fn reaches(p: &Project, from: u64, to: u64) -> bool {
    let mut seen = vec![from];
    let mut todo = vec![from];
    while let Some(id) = todo.pop() {
        let Some(t) = p.tracks.iter().find(|t| t.id == id) else { continue };
        for next in t.mixer.output.into_iter().chain(t.mixer.sends.iter().map(|s| s.to)) {
            if next == to {
                return true;
            }
            if !seen.contains(&next) {
                seen.push(next);
                todo.push(next);
            }
        }
    }
    false
}

/// A compressor's settings brought into range.
fn check_comp(c: &Comp) -> Comp {
    Comp {
        ratio: c.ratio.max(1.0),
        attack_ms: c.attack_ms.clamp(0.05, 1000.0),
        release_ms: c.release_ms.clamp(1.0, 5000.0),
        knee_db: c.knee_db.clamp(0.0, 24.0),
        sidechain_hpf_hz: c.sidechain_hpf_hz.clamp(0.0, 1000.0),
        lookahead_ms: c.lookahead_ms.clamp(0.0, 20.0),
        mix: c.mix.clamp(0.0, 1.0),
        ..*c
    }
}

fn check_dyn_band(b: &DynBand) -> Result<DynBand> {
    if !matches!(b.kind, EqKind::Peak | EqKind::LowShelf | EqKind::HighShelf) {
        bail!("a dynamic EQ band is a peak, low_shelf or high_shelf");
    }
    Ok(DynBand {
        hz: b.hz.clamp(20.0, 20000.0),
        q: b.q.clamp(0.1, 10.0),
        ratio: b.ratio.max(1.0),
        range_db: b.range_db.clamp(-24.0, 0.0),
        attack_ms: b.attack_ms.clamp(0.1, 500.0),
        release_ms: b.release_ms.clamp(5.0, 2000.0),
        ..*b
    })
}

fn check_reverb(r: &ReverbParams) -> ReverbParams {
    ReverbParams {
        room_size: r.room_size.clamp(0.0, 1.0),
        damping: r.damping.clamp(0.0, 1.0),
        predelay_ms: r.predelay_ms.clamp(0.0, 500.0),
        decay_s: r.decay_s.clamp(0.0, 12.0),
        width: r.width.clamp(0.0, 1.0),
        ..*r
    }
}

fn check_multiband(mb: &Multiband) -> Result<Multiband> {
    let x = &mb.crossovers_hz;
    if x.is_empty() || x.len() > 3 {
        bail!("a multiband compressor has 1 to 3 crossovers (2 to 4 bands)");
    }
    if x.windows(2).any(|w| w[1] <= w[0] * 1.5) || x.iter().any(|&f| !(30.0..=16000.0).contains(&f)) {
        bail!("crossovers go up from 30 Hz to 16 kHz, each at least half an octave above the last");
    }
    if mb.bands.len() != x.len() + 1 {
        bail!("{} crossovers make {} bands: give one setting per band, low to high", x.len(), x.len() + 1);
    }
    let bands = mb
        .bands
        .iter()
        .map(|b| MbBand {
            ratio: b.ratio.max(1.0),
            attack_ms: b.attack_ms.clamp(0.1, 1000.0),
            release_ms: b.release_ms.clamp(5.0, 5000.0),
            gain_db: b.gain_db.clamp(-12.0, 12.0),
            ..*b
        })
        .collect();
    Ok(Multiband { crossovers_hz: x.clone(), bands })
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

/// The automation lanes `track` or `bus` names: a track's, or the master's (either bus).
fn lanes_mut<'a>(p: &'a mut Project, track: &Option<TrackRef>, bus: Option<Bus>) -> Result<&'a mut Vec<Lane>> {
    match (track, bus) {
        (Some(_), Some(_)) => bail!("give either track or bus, not both"),
        (Some(r), None) => Ok(&mut track_mut(p, r)?.automation),
        (None, Some(_)) => Ok(&mut p.master.automation),
        (None, None) => bail!("automation needs a track, or bus \"master\""),
    }
}

/// What an add_automation moves, checked against the project.
fn automation_target(
    p: &Project,
    track: &Option<TrackRef>,
    bus: Option<Bus>,
    kind: Kind,
    insert: Option<u64>,
    param: &Option<String>,
    normalized: bool,
) -> Result<Target> {
    let owner = match (track, bus) {
        (Some(_), Some(_)) => bail!("give either track or bus, not both"),
        (Some(r), None) => Some(&p.tracks[p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?]),
        (None, Some(_)) => None,
        (None, None) => bail!("automation needs a track, or bus \"master\""),
    };
    let fixed = match kind {
        Kind::Volume => Some(Target::Volume),
        Kind::Pan => Some(Target::Pan),
        Kind::ReverbSend => Some(Target::ReverbSend),
        Kind::Param => None,
    };
    if let Some(t) = fixed {
        if owner.is_none() {
            bail!("the master's automation moves insert parameters only (target \"param\")");
        }
        if insert.is_some() || param.is_some() || normalized {
            bail!("{} automation takes no insert, param or normalized", t.label());
        }
        return Ok(t);
    }
    let param = param.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("param automation needs the parameter's name (or #id)"))?;
    match insert {
        Some(id) => {
            let (chain, _, _) = p.insert(id).ok_or_else(|| anyhow!("no insert #{id}"))?;
            match (owner, chain) {
                (Some(t), Chain::Track(c)) if c == t.id => {}
                (None, Chain::Master | Chain::Reverb) => {}
                (Some(t), _) => bail!("insert #{id} is not on track '{}'", t.name),
                (None, _) => bail!("insert #{id} is not on the master or the reverb return"),
            }
        }
        None => match owner.map(|t| (t, &t.instrument)) {
            None => bail!("the master's param automation needs the insert"),
            Some((t, Instrument::Vst3 { path, .. })) if path.ends_with(".rig.json") => {
                bail!("track '{}' plays a rig, whose stages can't be automated; automate an insert's parameter instead", t.name)
            }
            Some((_, Instrument::Vst3 { .. })) => {}
            Some((t, _)) => bail!("track '{}' has no VST3 instrument: give the insert whose parameter moves", t.name),
        },
    }
    Ok(Target::Param { insert, param: param.to_string() })
}

/// Remove the lanes' points in `[a, b)` (beats), and the lanes left without any.
fn clear_lanes(lanes: &mut Vec<Lane>, a: f64, b: f64) {
    for l in lanes.iter_mut() {
        l.clear(a, b);
    }
    lanes.retain(|l| !l.points.is_empty());
}

/// The VST3 instrument plugin a track's instrument-parameter lanes belong to.
fn automatable_plugin(i: &Instrument) -> Option<(&str, Option<&str>)> {
    match i {
        Instrument::Vst3 { path, class, .. } if !path.ends_with(".rig.json") => Some((path, class.as_deref())),
        _ => None,
    }
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
            let (instrument, file) = take_file(instrument);
            let clips = match file {
                Some(f) => vec![Clip::new(p.alloc_id(), f, 0.0)],
                None => vec![],
            };
            let clip_id = clips.first().map(|c| c.id);
            p.tracks.push(Track {
                id,
                name: name.clone(),
                color,
                instrument,
                notes: vec![],
                ccs: vec![],
                bends: vec![],
                mixer: Mixer::default(),
                meta: Default::default(),
                layer: String::new(),
                automation: vec![],
                clips,
            });
            Ok(match clip_id {
                Some(c) => json!({"track_id": id, "clip_id": c}),
                None => json!({"track_id": id}),
            })
        }
        Command::RemoveTrack { track } => {
            let i = p.track_index(track).ok_or_else(|| anyhow!("no track {track}"))?;
            let t = p.tracks.remove(i);
            for o in p.tracks.iter_mut() {
                if o.mixer.duck.map(|d| d.source_track) == Some(t.id) {
                    o.mixer.duck = None;
                }
                // what went to a removed bus goes to the master
                if o.mixer.output == Some(t.id) {
                    o.mixer.output = None;
                }
                o.mixer.sends.retain(|s| s.to != t.id);
            }
            Ok(json!({"removed": t.name}))
        }
        Command::AddBus { name, tracks, sends, mixer, color } => {
            let added = apply(p, &Command::AddTrack { name: name.clone(), instrument: Instrument::None, color: *color })?;
            let bus = TrackRef::Id(added["track_id"].as_u64().unwrap_or_default());
            for t in tracks {
                apply(p, &Command::SetMixer { track: t.clone(), mixer: MixerPatch { output: Some(Some(bus.clone())), ..Default::default() } })?;
            }
            for s in sends {
                let i = p.track_index(&s.track).ok_or_else(|| anyhow!("no track {}", s.track))?;
                let mut list: Vec<SendSpec> =
                    p.tracks[i].mixer.sends.iter().map(|x| SendSpec { to: TrackRef::Id(x.to), level: x.level }).collect();
                list.retain(|x| x.to != bus);
                list.push(SendSpec { to: bus.clone(), level: s.level });
                apply(p, &Command::SetMixer { track: s.track.clone(), mixer: MixerPatch { sends: Some(list), ..Default::default() } })?;
            }
            if let Some(m) = mixer {
                apply(p, &Command::SetMixer { track: bus.clone(), mixer: m.clone() })?;
            }
            Ok(json!({"track_id": added["track_id"], "routed": tracks.len(), "sending": sends.len()}))
        }
        Command::UpdateTrack { track, name, instrument, color, layer } => {
            if let Some(n) = name
                && p.tracks.iter().any(|t| t.name == *n) {
                    bail!("a track named '{n}' already exists");
                }
            p.track_index(track).ok_or_else(|| anyhow!("no track {track}"))?;
            let (instrument, file) = match instrument {
                Some(i) => {
                    let (i, f) = take_file(i);
                    (Some(i), f)
                }
                None => (None, None),
            };
            let clip_id = file.as_ref().map(|_| p.alloc_id());
            let t = track_mut(p, track)?;
            if let Some(n) = name {
                t.name = n.clone();
            }
            let mut dropped = 0;
            if let Some(i) = instrument {
                // another plugin (or none) has other parameters: the old one's lanes go with it
                if automatable_plugin(&i) != automatable_plugin(&t.instrument) {
                    let before = t.automation.len();
                    t.automation.retain(|l| !matches!(l.target, Target::Param { insert: None, .. }));
                    dropped = before - t.automation.len();
                }
                t.instrument = i;
            }
            // the file named with the instrument is what the track plays now
            if let (Some(f), Some(id)) = (file, clip_id) {
                t.clips = vec![Clip::new(id, f, 0.0)];
            }
            if let Some(c) = color {
                t.color = *c;
            }
            if let Some(l) = layer {
                t.layer = l.trim().to_lowercase();
            }
            let mut out = json!({"ok": true});
            if dropped > 0 {
                out["automation_lanes_dropped"] = json!(dropped);
            }
            if let Some(c) = clip_id {
                out["clip_id"] = json!(c);
            }
            Ok(out)
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
            let me = p.tracks[p.track_index(track).ok_or_else(|| anyhow!("no track {track}"))?].id;
            let output = match &m.output {
                Some(Some(r)) => Some(Some(route_to(p, me, r, "output")?)),
                Some(None) => Some(None),
                None => None,
            };
            let sends = match &m.sends {
                Some(list) => {
                    let mut v: Vec<AuxSend> = vec![];
                    for s in list {
                        let to = route_to(p, me, &s.to, "send")?;
                        if v.iter().any(|x| x.to == to) {
                            bail!("two sends to {}: give each receiving track once", s.to);
                        }
                        v.push(AuxSend { to, level: s.level.clamp(0.0, 1.0) });
                    }
                    Some(v)
                }
                None => None,
            };
            let dyn_eq = match &m.dyn_eq {
                Some(bands) => Some(bands.iter().map(check_dyn_band).collect::<Result<Vec<_>>>()?),
                None => None,
            };
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
                x.comp = v.map(|c| check_comp(&c));
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
            if let Some(v) = dyn_eq {
                x.dyn_eq = v;
            }
            if let Some(v) = m.deess {
                x.deess = v.map(|d| DeEss { hz: d.hz.clamp(1000.0, 16000.0), threshold_db: d.threshold_db, range_db: d.range_db.clamp(-24.0, 0.0) });
            }
            if let Some(v) = m.reverb {
                x.reverb = v.map(|r| ReverbFx { params: check_reverb(&r.params), mix: r.mix.clamp(0.0, 1.0) });
            }
            if let Some(v) = output {
                x.output = v;
            }
            if let Some(v) = sends {
                x.sends = v;
            }
            Ok(json!({"mixer": t.mixer}))
        }
        Command::SetMaster { master: m } => {
            let multiband = match &m.multiband {
                Some(Some(mb)) => Some(Some(check_multiband(mb)?)),
                Some(None) => Some(None),
                None => None,
            };
            let x = &mut p.master;
            if let Some(v) = m.glue {
                x.glue = v.map(|c| check_comp(&c));
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
                x.reverb = check_reverb(&v);
            }
            if let Some(v) = m.edge_fade_ms {
                x.edge_fade_ms = v.max(0.0);
            }
            if let Some(v) = m.tail_s {
                x.tail_s = v.clamp(0.0, 30.0);
            }
            if let Some(v) = m.limiter {
                x.limiter = Limiter {
                    lookahead_ms: v.lookahead_ms.clamp(0.5, 20.0),
                    release_ms: v.release_ms.clamp(0.0, 2000.0),
                    auto_release: v.auto_release,
                    clip_db: v.clip_db.clamp(0.0, 6.0),
                };
            }
            if let Some(v) = multiband {
                x.multiband = v;
            }
            if let Some(v) = m.mono_below_hz {
                x.mono_below_hz = v.clamp(0.0, 500.0);
            }
            if let Some(v) = m.width {
                x.width = v.clamp(0.0, 2.0);
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
            let lanes = match chain {
                Chain::Track(id) => p.tracks.iter_mut().find(|t| t.id == id).map(|t| &mut t.automation),
                Chain::Master | Chain::Reverb => Some(&mut p.master.automation),
            };
            if let Some(l) = lanes {
                l.retain(|l| l.target.insert() != Some(*insert));
            }
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
        Command::AddAutomation { track, bus, target, insert, param, normalized, points, replace } => {
            let target = automation_target(p, track, *bus, *target, *insert, param, *normalized)?;
            if points.is_empty() && !*replace {
                bail!("add_automation needs points");
            }
            let range = target.range(*normalized);
            let mut new = Vec::with_capacity(points.len());
            for s in points {
                if !s.beat.is_finite() || !s.value.is_finite() {
                    bail!("automation points need finite beats and values");
                }
                let beat = spec_beat(&tm, s.bar, s.beat);
                if beat < 0.0 {
                    bail!("an automation point at beat {beat}: the song starts at 0");
                }
                let value = range.map_or(s.value, |(lo, hi)| s.value.clamp(lo, hi));
                new.push(Point { beat, value, shape: s.shape });
            }
            let label = target.label();
            let lanes = lanes_mut(p, track, *bus)?;
            if new.is_empty() {
                let before = lanes.len();
                lanes.retain(|l| !l.target.same(&target));
                return Ok(json!({"lane": label, "removed": before > lanes.len()}));
            }
            let lane = match lanes.iter().position(|l| l.target.same(&target)) {
                Some(i) => &mut lanes[i],
                None => {
                    lanes.push(Lane::new(target, *normalized));
                    lanes.last_mut().unwrap()
                }
            };
            if *replace {
                lane.points.clear();
                lane.normalized = *normalized;
            }
            if lane.normalized != *normalized {
                let (has, give) = if lane.normalized { ("normalized values", "normalized: true") } else { ("display values", "normalized: false") };
                bail!("the {label} lane holds {has}: write them with {give}, or clear the lane first");
            }
            lane.write(new);
            Ok(json!({"lane": label, "points_in_lane": lane.points.len()}))
        }
        Command::ClearAutomation { track, bus, target, insert, param, from_bar, to_bar } => {
            let (a, b) = beat_range(&tm, *from_bar, *to_bar);
            let whole = from_bar.is_none() && to_bar.is_none();
            let lanes = lanes_mut(p, track, *bus)?;
            let mut removed = 0;
            for l in lanes.iter_mut() {
                let picked = target.is_none_or(|k| l.target.kind() == k)
                    && insert.is_none_or(|i| l.target.insert() == Some(i))
                    && param.as_deref().is_none_or(|n| matches!(&l.target, Target::Param { param: q, .. } if q.trim().eq_ignore_ascii_case(n.trim())));
                if picked {
                    removed += if whole { std::mem::take(&mut l.points).len() } else { l.clear(a, b) };
                }
            }
            lanes.retain(|l| !l.points.is_empty());
            Ok(json!({"removed": removed, "lanes": lanes.len()}))
        }
        Command::CopyBars { track, from_bar, bars, to_bar } => {
            let a = tm.bar_beat(*from_bar);
            let b = tm.bar_beat(from_bar + bars);
            let off = tm.bar_beat(*to_bar) - a;
            let idx: Vec<usize> = match track {
                Some(r) => vec![p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?],
                None => (0..p.tracks.len()).collect(),
            };
            let copy_lanes = |lanes: &mut Vec<Lane>| {
                for l in lanes.iter_mut() {
                    let seg = l.segment(a, b, off);
                    l.paste(seg, a + off, b + off);
                }
            };
            // the clips' audio in the range, as new clips
            let pieces: Vec<(usize, Clip)> = idx.iter().flat_map(|&i| p.tracks[i].clips.iter().filter_map(|c| c.piece(&tm, a, b)).map(move |c| (i, c))).collect();
            let clips_copied = pieces.len();
            for (i, mut c) in pieces {
                c.id = p.alloc_id();
                c.start += off;
                p.tracks[i].clips.push(c);
            }
            let mut copied = 0;
            for &i in &idx {
                let t = &mut p.tracks[i];
                let notes: Vec<Note> = t
                    .notes
                    .iter()
                    .filter(|n| n.start >= a - 1e-9 && n.start < b - 1e-9)
                    .map(|n| Note { start: n.start + off, ..*n })
                    .collect();
                let ccs: Vec<CcPoint> =
                    t.ccs.iter().filter(|c| c.beat >= a - 1e-9 && c.beat < b - 1e-9).map(|c| CcPoint { beat: c.beat + off, ..*c }).collect();
                let bends: Vec<BendPoint> =
                    t.bends.iter().filter(|c| c.beat >= a - 1e-9 && c.beat < b - 1e-9).map(|c| BendPoint { beat: c.beat + off, ..*c }).collect();
                copied += notes.len();
                t.notes.extend(notes);
                t.ccs.extend(ccs);
                t.bends.extend(bends);
                t.notes.sort_by(|x, y| x.start.partial_cmp(&y.start).unwrap());
                t.ccs.sort_by(|x, y| x.beat.partial_cmp(&y.beat).unwrap());
                t.bends.sort_by(|x, y| x.beat.partial_cmp(&y.beat).unwrap());
                sort_clips(t);
                copy_lanes(&mut t.automation);
            }
            if track.is_none() {
                copy_lanes(&mut p.master.automation);
            }
            Ok(if clips_copied > 0 { json!({"notes_copied": copied, "clips_copied": clips_copied}) } else { json!({"notes_copied": copied}) })
        }
        Command::ClearBars { track, from_bar, bars } => {
            let a = tm.bar_beat(*from_bar);
            let b = tm.bar_beat(from_bar + bars);
            let idx: Vec<usize> = match track {
                Some(r) => vec![p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?],
                None => (0..p.tracks.len()).collect(),
            };
            // a clip over the whole range leaves a piece each side: the one after it is a new clip
            let spanning = idx.iter().flat_map(|&i| &p.tracks[i].clips).filter(|c| c.piece(&tm, f64::NEG_INFINITY, a).is_some() && c.piece(&tm, b, f64::INFINITY).is_some()).count();
            let mut fresh: Vec<u64> = (0..spanning).map(|_| p.alloc_id()).collect();
            let mut removed = 0;
            for i in idx {
                let t = &mut p.tracks[i];
                let mut clips = Vec::with_capacity(t.clips.len());
                for c in &t.clips {
                    let front = c.piece(&tm, f64::NEG_INFINITY, a);
                    let back = c.piece(&tm, b, f64::INFINITY);
                    match (front, back) {
                        (Some(f), Some(mut k)) => {
                            k.id = fresh.pop().ok_or_else(|| anyhow!("clip ids ran out"))?;
                            clips.extend([f, k]);
                        }
                        (Some(x), None) | (None, Some(x)) => clips.push(x),
                        (None, None) => {}
                    }
                }
                t.clips = clips;
                sort_clips(t);
                let before = t.notes.len();
                t.notes.retain(|n| !(n.start >= a - 1e-9 && n.start < b - 1e-9));
                t.ccs.retain(|c| !(c.beat >= a - 1e-9 && c.beat < b - 1e-9));
                t.bends.retain(|c| !(c.beat >= a - 1e-9 && c.beat < b - 1e-9));
                removed += before - t.notes.len();
                clear_lanes(&mut t.automation, a, b);
            }
            if track.is_none() {
                clear_lanes(&mut p.master.automation, a, b);
            }
            Ok(json!({"notes_removed": removed}))
        }
        Command::AddClip { track, path, bar, beat, offset, length, gain_db, fade_in, fade_out } => {
            let start = spec_beat(&tm, *bar, *beat);
            let c = checked(Clip { id: 0, path: path.clone(), start, offset: *offset, length: *length, gain_db: *gain_db, fade_in: *fade_in, fade_out: *fade_out })?;
            let i = audio_track(p, track)?;
            let c = Clip { id: p.alloc_id(), ..c };
            let id = c.id;
            let t = &mut p.tracks[i];
            t.clips.push(c);
            sort_clips(t);
            Ok(json!({"clip_id": id, "clips_in_track": t.clips.len()}))
        }
        Command::UpdateClip { clip, track, bar, beat, trim, path, offset, length, gain_db, fade_in, fade_out } => {
            let (ti, ci, old) = p.clip(*clip).ok_or_else(|| anyhow!("no clip #{clip}"))?;
            let mut c = old.clone();
            let to = (bar.is_some() || beat.is_some()).then(|| spec_beat(&tm, *bar, beat.unwrap_or(0.0)));
            if *trim {
                let Some(s) = to else { bail!("trim moves the clip's start: give bar and/or beat") };
                if offset.is_some() {
                    bail!("give trim or offset, not both: trim works the offset out");
                }
                let d = tm.sec(s) - tm.sec(c.start);
                if c.offset + d < -1e-9 {
                    let earliest = tm.beat(tm.sec(c.start) - c.offset);
                    bail!("clip #{clip} can't start before its file does: beat {earliest:.3} at the earliest");
                }
                c.offset = (c.offset + d).max(0.0);
                if let Some(l) = c.length {
                    if l - d <= 1e-9 {
                        bail!("clip #{clip} would have nothing left: it ends before beat {s}");
                    }
                    c.length = Some(l - d);
                }
                c.start = s;
            } else if let Some(s) = to {
                c.start = s;
            }
            if let Some(v) = path {
                c.path = v.clone();
            }
            if let Some(v) = offset {
                c.offset = *v;
            }
            if let Some(v) = length {
                c.length = *v;
            }
            if let Some(v) = gain_db {
                c.gain_db = *v;
            }
            if let Some(v) = fade_in {
                c.fade_in = *v;
            }
            if let Some(v) = fade_out {
                c.fade_out = *v;
            }
            let c = checked(c)?;
            let dest = match track {
                Some(r) => audio_track(p, r)?,
                None => ti,
            };
            p.tracks[ti].clips.remove(ci);
            let t = &mut p.tracks[dest];
            t.clips.push(c.clone());
            sort_clips(t);
            Ok(json!({"clip": c, "track": t.name}))
        }
        Command::RemoveClip { clip } => {
            let (ti, ci, _) = p.clip(*clip).ok_or_else(|| anyhow!("no clip #{clip}"))?;
            let t = &mut p.tracks[ti];
            let gone = t.clips.remove(ci);
            Ok(json!({"removed": gone.file_name(), "track": t.name}))
        }
        Command::SplitClip { clip, bar, beat } => {
            let at = spec_beat(&tm, *bar, *beat);
            let (ti, ci, c) = p.clip(*clip).ok_or_else(|| anyhow!("no clip #{clip}"))?;
            let (Some(front), Some(back)) = (c.piece(&tm, f64::NEG_INFINITY, at), c.piece(&tm, at, f64::INFINITY)) else {
                bail!("beat {at} is not inside clip #{clip} (it starts at beat {})", c.start);
            };
            let back = Clip { id: p.alloc_id(), ..back };
            let id = back.id;
            let t = &mut p.tracks[ti];
            t.clips[ci] = front;
            t.clips.push(back);
            sort_clips(t);
            Ok(json!({"clip_id": id}))
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

    #[test]
    fn reverb_settings_left_out_take_their_defaults() -> Result<()> {
        let mut p = Project::new("t", 120.0, 4);
        run(&mut p, json!({"op": "add_track", "name": "lead"}))?;
        run(&mut p, json!({"op": "set_mixer", "track": "lead", "mixer": {"reverb": {"algorithm": "plate", "mix": 0.2}}}))?;
        let fx = p.tracks[0].mixer.reverb.unwrap();
        let d = ReverbParams::default();
        assert_eq!((fx.params.algorithm, fx.params.room_size, fx.params.hpf_hz, fx.mix), (ReverbAlgorithm::Plate, d.room_size, d.hpf_hz, 0.2));
        run(&mut p, json!({"op": "set_master", "master": {"reverb": {"decay_s": 2.5}}}))?;
        let r = p.master.reverb;
        assert_eq!((r.decay_s, r.lpf_hz, r.return_db), (2.5, d.lpf_hz, d.return_db));
        // left out, the algorithm is still the classic one older projects used
        assert_eq!(r.algorithm, ReverbAlgorithm::Classic);
        Ok(())
    }

    #[test]
    fn automation() -> Result<()> {
        let mut p = Project::new("t", 120.0, 16);
        let vital = json!({"kind": "vst3", "path": "C:/Program Files/Common Files/VST3/Vital.vst3"});
        run(&mut p, json!({"op": "add_track", "name": "pad", "instrument": vital}))?;
        run(&mut p, json!({"op": "add_track", "name": "keys"}))?;
        let sm = "C:/Program Files/Common Files/VST3/ValhallaSupermassive.vst3";
        let ins = run(&mut p, json!({"op": "add_insert", "track": "pad", "path": sm}))?["insert_id"].as_u64().unwrap();
        let rev = run(&mut p, json!({"op": "add_insert", "bus": "reverb", "path": sm}))?["insert_id"].as_u64().unwrap();

        // a fade in over bar 1, clamped to the fader's range; then a swell written over part of it
        let fade = json!({"op": "add_automation", "track": "pad", "target": "volume",
            "points": [{"bar": 1, "beat": 0, "value": -200}, {"bar": 2, "beat": 0, "value": 0, "shape": "smooth"}, {"bar": 5, "value": -6}]});
        assert_eq!(run(&mut p, fade)?["lane"], "volume");
        let pad = &p.tracks[0].automation[0];
        assert_eq!(pad.points.iter().map(|q| (q.beat, q.value)).collect::<Vec<_>>(), [(0.0, -90.0), (4.0, 0.0), (16.0, -6.0)]);
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "volume", "points": [{"beat": 8, "value": -3}, {"beat": 2, "value": -12}]}))?;
        let beats: Vec<f64> = p.tracks[0].automation[0].points.iter().map(|q| q.beat).collect();
        assert_eq!(beats, [0.0, 2.0, 8.0, 16.0]);

        // parameters: the instrument's, an insert's, the reverb return's
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "param", "param": "Filter 1 Cutoff", "normalized": true,
            "points": [{"beat": 0, "value": 0.2}, {"bar": 3, "value": 1.5}]}))?;
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "param", "insert": ins, "param": "Mix", "points": [{"beat": 0, "value": 30}]}))?;
        run(&mut p, json!({"op": "add_automation", "bus": "master", "target": "param", "insert": rev, "param": "Mix", "points": [{"beat": 0, "value": 100}]}))?;
        assert_eq!(p.tracks[0].automation.len(), 3);
        assert_eq!(p.tracks[0].automation[1].points[1].value, 1.0);
        // the same parameter whatever its case
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "param", "insert": ins, "param": "mix", "points": [{"beat": 4, "value": 50}]}))?;
        assert_eq!(p.tracks[0].automation[2].points.len(), 2);
        // replace: the points become the whole lane, in either kind of value
        let mix = |pts: Value, normalized: bool| {
            json!({"op": "add_automation", "track": "pad", "target": "param", "insert": ins, "param": "Mix", "normalized": normalized, "replace": true, "points": pts})
        };
        run(&mut p, mix(json!([{"beat": 2, "value": 0.5, "shape": "hold"}, {"beat": 6, "value": 0.25}]), true))?;
        let l = &p.tracks[0].automation[2];
        assert!(l.normalized);
        assert_eq!(l.points.iter().map(|q| (q.beat, q.value, q.shape)).collect::<Vec<_>>(), [(2.0, 0.5, Shape::Hold), (6.0, 0.25, Shape::Linear)]);
        run(&mut p, mix(json!([{"beat": 0, "value": 30}, {"beat": 4, "value": 50}]), false))?;
        assert!(!p.tracks[0].automation[2].normalized);
        assert_eq!(p.tracks[0].automation[2].points.len(), 2);
        // and no points delete just that lane (not the instrument's parameter of the same name)
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "param", "param": "Mix", "normalized": true, "points": [{"beat": 0, "value": 0.5}]}))?;
        assert_eq!(run(&mut p, mix(json!([]), false))?["removed"], true);
        assert_eq!(p.tracks[0].automation.iter().map(|l| l.target.label()).collect::<Vec<_>>(), ["volume", "Filter 1 Cutoff", "Mix"]);
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "param", "param": "Mix", "replace": true, "points": []}))?;
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "param", "insert": ins, "param": "Mix", "points": [{"beat": 0, "value": 30}, {"beat": 4, "value": 50}]}))?;

        // what can't be automated
        for bad in [
            json!({"op": "add_automation", "track": "keys", "target": "param", "param": "Cutoff", "points": [{"beat": 0, "value": 1}]}),
            json!({"op": "add_automation", "track": "keys", "target": "param", "insert": ins, "param": "Mix", "points": [{"beat": 0, "value": 1}]}),
            json!({"op": "add_automation", "bus": "master", "target": "volume", "points": [{"beat": 0, "value": 0}]}),
            json!({"op": "add_automation", "bus": "master", "target": "param", "insert": ins, "param": "Mix", "points": [{"beat": 0, "value": 1}]}),
            json!({"op": "add_automation", "track": "pad", "target": "pan", "param": "x", "points": [{"beat": 0, "value": 0}]}),
            json!({"op": "add_automation", "track": "pad", "target": "pan", "points": []}),
            json!({"op": "add_automation", "track": "pad", "target": "param", "param": "Filter 1 Cutoff", "points": [{"beat": 0, "value": 800}]}),
        ] {
            assert!(run(&mut p, bad.clone()).is_err(), "{bad}");
        }

        // copying bars 1-4 to 9-12 copies the curves too; clearing bars removes points
        run(&mut p, json!({"op": "copy_bars", "from_bar": 1, "bars": 4, "to_bar": 9}))?;
        let vol = &p.tracks[0].automation[0];
        for b in [0.0, 1.0, 2.0, 5.0, 8.0, 15.5] {
            assert!((vol.value_at(b + 32.0).unwrap() - vol.value_at(b).unwrap()).abs() < 1e-9, "beat {b}");
        }
        assert_eq!(p.master.automation[0].points.len(), 1);
        run(&mut p, json!({"op": "clear_bars", "track": "pad", "from_bar": 9, "bars": 8}))?;
        assert!(p.tracks[0].automation[0].points.iter().all(|q| q.beat < 32.0));

        // clear_automation: a range of one lane, then whole lanes
        let r = run(&mut p, json!({"op": "clear_automation", "track": "pad", "target": "volume", "from_bar": 1, "to_bar": 1}))?;
        assert_eq!(r["removed"], 2);
        run(&mut p, json!({"op": "clear_automation", "track": "pad", "target": "param", "param": "FILTER 1 CUTOFF"}))?;
        assert_eq!(p.tracks[0].automation.iter().map(|l| l.target.label()).collect::<Vec<_>>(), ["volume".to_string(), format!("Mix (insert #{ins})")]);

        // lanes go with their insert, and with the instrument plugin
        run(&mut p, json!({"op": "remove_insert", "insert": ins}))?;
        run(&mut p, json!({"op": "remove_insert", "insert": rev}))?;
        assert_eq!(p.tracks[0].automation.len(), 1);
        assert!(p.master.automation.is_empty());
        run(&mut p, json!({"op": "add_automation", "track": "pad", "target": "param", "param": "Cutoff", "points": [{"beat": 0, "value": 300}]}))?;
        let same = json!({"op": "update_track", "track": "pad", "instrument": {"kind": "vst3", "path": "C:/Program Files/Common Files/VST3/Vital.vst3", "preset": "x.vstpreset"}});
        assert!(run(&mut p, same)?.get("automation_lanes_dropped").is_none());
        let r = run(&mut p, json!({"op": "update_track", "track": "pad", "instrument": {"kind": "none"}}))?;
        assert_eq!(r["automation_lanes_dropped"], 1);
        assert_eq!(p.tracks[0].automation[0].target, Target::Volume);

        // saved and loaded with the project
        let back: Project = serde_json::from_value(serde_json::to_value(&p)?)?;
        assert_eq!(back.tracks[0].automation, p.tracks[0].automation);
        Ok(())
    }

    #[test]
    fn clips() -> Result<()> {
        // 120 bpm: a beat is half a second, a bar two seconds
        let mut p = Project::new("t", 120.0, 16);
        run(&mut p, json!({"op": "add_track", "name": "keys", "instrument": {"kind": "sfz", "path": "x.sfz"}}))?;
        run(&mut p, json!({"op": "add_track", "name": "fx"}))?;
        // an audio file given with the instrument is a clip at the top
        let r = run(&mut p, json!({"op": "add_track", "name": "choir", "instrument": {"kind": "audio", "path": "stems/choir.wav"}}))?;
        let choir = r["clip_id"].as_u64().unwrap();
        let t = &p.tracks[2];
        assert_eq!(t.instrument, Instrument::Audio { path: String::new() });
        assert_eq!(t.clips, [Clip::new(choir, "stems/choir.wav", 0.0)]);

        // anywhere, trimmed, with gain and fades; a track without an instrument becomes an audio track
        let hit = run(&mut p, json!({"op": "add_clip", "track": "fx", "path": "one-shots/hit.wav", "bar": 5, "beat": 2,
            "offset": 0.25, "length": 1.5, "gain_db": -40.5, "fade_out": 0.5}))?["clip_id"].as_u64().unwrap();
        let c = p.clip(hit).unwrap().2.clone();
        assert!(p.tracks[1].is_audio());
        assert_eq!((c.start, c.offset, c.length, c.gain_db, c.fade_in, c.fade_out), (18.0, 0.25, Some(1.5), -40.5, 0.0, 0.5));
        for bad in [
            json!({"op": "add_clip", "track": "fx", "path": " "}),
            json!({"op": "add_clip", "track": "fx", "path": "a.wav", "beat": -1}),
            json!({"op": "add_clip", "track": "fx", "path": "a.wav", "offset": -0.5}),
            json!({"op": "add_clip", "track": "fx", "path": "a.wav", "length": 0}),
            json!({"op": "add_clip", "track": "fx", "path": "a.wav", "fade_in": -1}),
            json!({"op": "update_clip", "clip": 999, "gain_db": 1}),
        ] {
            assert!(run(&mut p, bad.clone()).is_err(), "{bad}");
        }
        let e = run(&mut p, json!({"op": "add_clip", "track": "keys", "path": "a.wav"})).unwrap_err().to_string();
        assert!(e.contains("audio track"), "{e}");

        // moved; its start trimmed later (the audio stays put) and back earlier, but not before the file starts
        run(&mut p, json!({"op": "update_clip", "clip": hit, "bar": 3}))?;
        assert_eq!(p.clip(hit).unwrap().2.start, 8.0);
        run(&mut p, json!({"op": "update_clip", "clip": hit, "beat": 8.5, "trim": true}))?;
        let c = p.clip(hit).unwrap().2.clone();
        assert_eq!((c.start, c.offset, c.length), (8.5, 0.5, Some(1.25)));
        let e = run(&mut p, json!({"op": "update_clip", "clip": hit, "beat": 7.0, "trim": true})).unwrap_err().to_string();
        assert!(e.contains("before its file"), "{e}");
        assert!(run(&mut p, json!({"op": "update_clip", "clip": hit, "beat": 12.0, "trim": true})).is_err(), "nothing left");
        run(&mut p, json!({"op": "update_clip", "clip": hit, "beat": 7.5, "trim": true}))?;
        let c = p.clip(hit).unwrap().2.clone();
        assert_eq!((c.start, c.offset, c.length), (7.5, 0.0, Some(1.75)));
        // gain is kept in range; length null plays to the end of the file
        run(&mut p, json!({"op": "update_clip", "clip": hit, "gain_db": 99, "length": null, "fade_in": 0.1}))?;
        let c = p.clip(hit).unwrap().2.clone();
        assert_eq!((c.gain_db, c.length, c.fade_in, c.fade_out), (clip::MAX_GAIN_DB, None, 0.1, 0.5));

        // split: the part after the cut is a new clip; the audio plays on exactly where it was
        run(&mut p, json!({"op": "update_clip", "clip": hit, "length": 4.0}))?;
        let back = run(&mut p, json!({"op": "split_clip", "clip": hit, "bar": 3, "beat": 1.5}))?["clip_id"].as_u64().unwrap();
        let (a, b) = (p.clip(hit).unwrap().2.clone(), p.clip(back).unwrap().2.clone());
        assert_eq!((a.start, a.length, a.fade_in, a.fade_out), (7.5, Some(1.0), 0.1, 0.0));
        assert_eq!((b.start, b.offset, b.length, b.fade_in, b.fade_out), (9.5, 1.0, Some(3.0), 0.0, 0.5));
        assert!(run(&mut p, json!({"op": "split_clip", "clip": back, "beat": 9.5})).is_err(), "on its edge");
        assert!(run(&mut p, json!({"op": "split_clip", "clip": back, "beat": 20})).is_err(), "past its end");

        // to another track, but not one that plays an instrument
        run(&mut p, json!({"op": "update_clip", "clip": back, "track": "choir"}))?;
        assert_eq!(p.clip(back).unwrap().0, 2);
        assert!(run(&mut p, json!({"op": "update_clip", "clip": back, "track": "keys"})).is_err());
        run(&mut p, json!({"op": "remove_clip", "clip": back}))?;
        assert!(p.clip(back).is_none());

        // clear_bars cuts clips at its edges: the choir (to the end of its file) loses bars 2-3
        run(&mut p, json!({"op": "clear_bars", "track": "choir", "from_bar": 2, "bars": 2}))?;
        let cl = &p.tracks[2].clips;
        assert_eq!(cl.len(), 2);
        assert_eq!((cl[0].id, cl[0].start, cl[0].offset, cl[0].length), (choir, 0.0, 0.0, Some(2.0)));
        assert_eq!((cl[1].start, cl[1].offset, cl[1].length), (12.0, 6.0, None));
        let ids: Vec<u64> = p.tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.id)).collect();
        assert!(!ids.iter().any(|i| p.tracks.iter().any(|t| t.id == *i)), "clip ids are the project's own");
        // copy_bars copies the audio in the range as new clips
        let r = run(&mut p, json!({"op": "copy_bars", "track": "choir", "from_bar": 1, "bars": 1, "to_bar": 9}))?;
        assert_eq!(r["clips_copied"], 1);
        let copy = p.tracks[2].clips.iter().find(|c| c.start == 32.0).expect("bar 9");
        assert_eq!((copy.offset, copy.length), (0.0, Some(2.0)));

        // update_track with an audio file: the track plays that file now
        let r = run(&mut p, json!({"op": "update_track", "track": "choir", "instrument": {"kind": "audio", "path": "stems/choir2.wav"}}))?;
        assert_eq!(p.tracks[2].clips, [Clip::new(r["clip_id"].as_u64().unwrap(), "stems/choir2.wav", 0.0)]);
        // and another instrument keeps the clips (silent), as an audio track keeps its notes
        run(&mut p, json!({"op": "update_track", "track": "choir", "instrument": {"kind": "none"}}))?;
        assert_eq!(p.tracks[2].clips.len(), 1);

        // saved and loaded; a project from before clips opens with its audio file as a clip
        let back: Project = serde_json::from_value(serde_json::to_value(&p)?)?;
        assert_eq!(serde_json::to_value(&back)?, serde_json::to_value(&p)?);
        let mut old = serde_json::to_value(Project::new("old", 90.0, 4))?;
        old["next_id"] = json!(3);
        old["tracks"] = json!([{"id": 2, "name": "stem", "instrument": {"kind": "audio", "path": "stems/a.wav"}}]);
        let old: Project = serde_json::from_value(old)?;
        assert_eq!(old.tracks[0].instrument, Instrument::Audio { path: String::new() });
        assert_eq!(old.tracks[0].clips, [Clip::new(3, "stems/a.wav", 0.0)]);
        assert_eq!(old.next_id, 4);
        assert_eq!(serde_json::to_value(&old)?["tracks"][0]["instrument"], json!({"kind": "audio"}));
        Ok(())
    }
}
