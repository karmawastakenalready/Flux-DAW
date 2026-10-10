use crate::clip::Clip;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A whole song. Times are in beats (quarter notes) from the top; bars are 1-based.
///
/// Read through [`ProjectFile`], which sets what a file leaves out and brings older files up to date.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "ProjectFile")]
pub struct Project {
    pub name: String,
    pub sample_rate: u32,
    /// Stepped tempo map: each point holds from its bar's downbeat until the next point.
    pub tempo: Vec<TempoPoint>,
    pub time_signature: (u32, u32),
    pub key: String,
    /// Song length in bars (the loop length when `looped`).
    pub bars: u32,
    pub looped: bool,
    pub sections: Vec<Section>,
    /// The chord track: what harmony the song means at each point, independent of how the parts voice it.
    /// Composing tools (voice leading, bass lines) and the score linter read it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub harmony: Vec<ChordMark>,
    pub tracks: Vec<Track>,
    pub master: Master,
    pub next_id: u64,
    /// Free-form notes for agents/humans (brief, intentions, todo).
    pub notes: String,
    /// Timeline comments ("bar 12 the harp is too loud"). Annotations, not content: they live outside the
    /// undo history and never change the sound or the revision.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,
}

/// A project as a file holds it: what it leaves out takes its default.
#[derive(Deserialize)]
struct ProjectFile {
    name: String,
    #[serde(default = "default_sr")]
    sample_rate: u32,
    tempo: Vec<TempoPoint>,
    time_signature: (u32, u32),
    #[serde(default)]
    key: String,
    bars: u32,
    #[serde(default)]
    looped: bool,
    #[serde(default)]
    sections: Vec<Section>,
    #[serde(default)]
    harmony: Vec<ChordMark>,
    #[serde(default)]
    tracks: Vec<Track>,
    #[serde(default)]
    master: Master,
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    comments: Vec<Comment>,
}

impl From<ProjectFile> for Project {
    fn from(f: ProjectFile) -> Project {
        let ProjectFile { name, sample_rate, tempo, time_signature, key, bars, looped, sections, harmony, tracks, master, next_id, notes, comments } =
            f;
        let mut p = Project { name, sample_rate, tempo, time_signature, key, bars, looped, sections, harmony, tracks, master, next_id, notes, comments };
        // before clips, an audio track played one file from the top
        for i in 0..p.tracks.len() {
            if let Instrument::Audio { path } = &mut p.tracks[i].instrument
                && !path.is_empty()
            {
                let path = std::mem::take(path);
                let id = p.alloc_id();
                p.tracks[i].clips.insert(0, Clip::new(id, path, 0.0));
            }
        }
        p
    }
}

/// A note pinned to the timeline, by a human (from the GUI) or the agent, with an optional reply thread.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Comment {
    pub id: u64,
    pub bar: u32,
    /// Offset inside the bar in beats.
    #[serde(default)]
    pub beat: f64,
    /// Last bar of a range comment (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_bar: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u64>,
    pub text: String,
    pub author: crate::session::Source,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replies: Vec<Reply>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Reply {
    pub author: crate::session::Source,
    pub text: String,
}

fn is_zero(x: &f64) -> bool {
    *x == 0.0
}

fn is_one(x: &f64) -> bool {
    *x == 1.0
}

fn d_one() -> f64 {
    1.0
}

fn default_sr() -> u32 {
    44100
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TempoPoint {
    pub bar: u32,
    pub bpm: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Section {
    pub name: String,
    pub start_bar: u32,
    pub bars: u32,
}

/// A chord on the chord track; it holds until the next mark.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChordMark {
    /// 1-based bar.
    pub bar: u32,
    /// Offset into the bar in beats (quarter notes).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub beat: f64,
    /// Chord symbol, e.g. "Am", "F#m7b5", "C/E", "Dsus4"; "N.C." for no chord.
    pub chord: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub color: [u8; 3],
    pub instrument: Instrument,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default)]
    pub ccs: Vec<CcPoint>,
    #[serde(default)]
    pub bends: Vec<BendPoint>,
    #[serde(default)]
    pub mixer: Mixer,
    /// Hints carried over from imports (e.g. the SFZ that originally rendered an audio stem).
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
    /// Game layer for layered stem export ("base", "melody", "percussion", ...); empty means "base".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layer: String,
    /// Volume, pan, reverb send and plugin parameters over time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation: Vec<crate::automation::Lane>,
    /// Audio clips, in order of their starts: what an audio track plays (other tracks keep theirs silent, as an
    /// audio track keeps its notes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn layer_name(&self) -> &str {
        if self.layer.is_empty() { "base" } else { &self.layer }
    }

    /// Whether it plays audio clips.
    pub fn is_audio(&self) -> bool {
        matches!(self.instrument, Instrument::Audio { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Instrument {
    /// Any SFZ file, played by the sfizz library.
    Sfz {
        path: String,
        /// CC values sent at time 0 (instrument "patch" settings).
        #[serde(default, deserialize_with = "cc_map")]
        init_cc: BTreeMap<u8, u8>,
    },
    /// A VST3 instrument plugin, hosted natively in a plugin-host process.
    ///
    /// Before each render the plugin is set to `preset` (or its default state), then `params` are applied.
    Vst3 {
        /// The `.vst3` bundle (or a `.rig.json` SFZ + effects rig).
        path: String,
        /// Plugin parameters by name: numbers in display units (dB, Hz, %), text ("Sine", "250 ms"), or
        /// `{"normalized": x}`.
        #[serde(default)]
        params: BTreeMap<String, serde_json::Value>,
        /// A `.vstpreset` with the plugin's full state (saved from its editor, a factory program, or a file).
        #[serde(default)]
        preset: Option<String>,
        /// What the preset is called (a factory program or a vendor preset's name), for people.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        preset_name: String,
        /// Which class of a multi-plugin bundle (name or class id); None = the first instrument/effect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        class: Option<String>,
    },
    /// Plays the track's audio clips (`Track::clips`): stems, recordings, one-shots, imports.
    Audio {
        /// A file to play from the top, as a clip. Projects stored audio tracks this way before clips; reading one,
        /// or giving a track this instrument, turns the file into a clip, so a track's own is always empty.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        path: String,
    },
    /// No sound (a MIDI / sketch track).
    None,
}

/// `{"1": 90}` → {1: 90}. Internally tagged enums buffer their fields, which loses serde_json's
/// usual string-to-integer map key conversion, so the keys are parsed here.
fn cc_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<u8, u8>, D::Error> {
    use serde::de::Error;
    let raw = BTreeMap::<String, u8>::deserialize(d)?;
    raw.into_iter()
        .map(|(k, v)| k.trim().parse::<u8>().map(|k| (k, v)).map_err(|_| D::Error::custom(format!("CC number {k:?} is not 0-255"))))
        .collect()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Note {
    /// Start, in beats from the top.
    pub start: f64,
    /// Length in beats.
    pub dur: f64,
    pub pitch: u8,
    pub vel: u8,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct CcPoint {
    pub beat: f64,
    pub cc: u8,
    pub value: u8,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct BendPoint {
    pub beat: f64,
    /// -8192 .. 8191
    pub value: i16,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EqKind {
    Peak,
    LowShelf,
    HighShelf,
    /// Cuts a narrow band out at `hz` (`q` sets how narrow; `db` is ignored).
    Notch,
    /// Tilts the whole spectrum around `hz`: `db` above it and `-db` below, through gentle shelves (`q` is ignored).
    Tilt,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct EqBand {
    pub kind: EqKind,
    pub hz: f64,
    pub db: f64,
    #[serde(default = "default_q")]
    pub q: f64,
}

fn default_q() -> f64 {
    0.9
}

/// A compressor. Left out, the newer settings (knee, detector, sidechain filter, lookahead, auto release, mix) take
/// the values that make it the plain feed-forward peak compressor older projects used.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Comp {
    /// dBFS, or dB relative to the signal's loud level (95th pct short-term RMS) when `relative`.
    pub threshold_db: f64,
    #[serde(default)]
    pub relative: bool,
    pub ratio: f64,
    #[serde(default = "d_attack")]
    pub attack_ms: f64,
    /// With `auto_release`, the release after sustained compression; short peaks recover faster.
    #[serde(default = "d_release")]
    pub release_ms: f64,
    #[serde(default)]
    pub makeup_db: f64,
    /// Soft knee (dB): the ratio eases in over this range around the threshold; 0 = a hard knee.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub knee_db: f64,
    /// What it listens to: peaks (catches transients) or RMS (follows loudness, smoother on buses).
    #[serde(default, skip_serializing_if = "Detector::is_peak")]
    pub detector: Detector,
    /// The detector hears the signal high-passed here (Hz), so the bass doesn't pump it; 0 = off.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sidechain_hpf_hz: f64,
    /// The audio runs this far behind the detector (ms), so even a fast transient is caught; 0 = none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lookahead_ms: f64,
    /// Program-dependent release: fast after short peaks, `release_ms` after sustained compression.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_release: bool,
    /// Parallel compression: the compressed signal's share of the output (1 = all of it, 0.5 = an even blend
    /// with the uncompressed signal).
    #[serde(default = "d_one", skip_serializing_if = "is_one")]
    pub mix: f64,
}

impl Default for Comp {
    fn default() -> Self {
        Comp {
            threshold_db: -18.0,
            relative: false,
            ratio: 2.0,
            attack_ms: d_attack(),
            release_ms: d_release(),
            makeup_db: 0.0,
            knee_db: 0.0,
            detector: Detector::Peak,
            sidechain_hpf_hz: 0.0,
            lookahead_ms: 0.0,
            auto_release: false,
            mix: 1.0,
        }
    }
}

/// What a compressor's level detector follows.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Detector {
    /// The louder channel's instantaneous level.
    #[default]
    Peak,
    /// The mean power over about 10 ms.
    Rms,
}

impl Detector {
    fn is_peak(&self) -> bool {
        *self == Detector::Peak
    }
}

/// A dynamic EQ band: turns its frequency range down only while that range is loud (a resonance that pops out,
/// a boomy note), by up to `range_db`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct DynBand {
    /// Peak, low_shelf or high_shelf.
    pub kind: EqKind,
    pub hz: f64,
    #[serde(default = "default_q")]
    pub q: f64,
    /// Where it starts acting: dB relative to the band's loud level (95th pct of its short-term RMS) when
    /// `relative`, else dBFS.
    pub threshold_db: f64,
    #[serde(default = "d_true")]
    pub relative: bool,
    #[serde(default = "d_dyn_ratio")]
    pub ratio: f64,
    /// The most it turns the band down (dB, negative).
    #[serde(default = "d_dyn_range")]
    pub range_db: f64,
    #[serde(default = "d_dyn_attack")]
    pub attack_ms: f64,
    #[serde(default = "d_dyn_release")]
    pub release_ms: f64,
}

fn d_true() -> bool {
    true
}
fn d_dyn_ratio() -> f64 {
    3.0
}
fn d_dyn_range() -> f64 {
    -6.0
}
fn d_dyn_attack() -> f64 {
    5.0
}
fn d_dyn_release() -> f64 {
    80.0
}

/// A de-esser: the band above `hz` is turned down while it's loud (sibilance, cymbal splash, pick noise).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct DeEss {
    #[serde(default = "d_deess_hz")]
    pub hz: f64,
    /// dB relative to the band's loud level: lower catches more.
    #[serde(default = "d_deess_threshold")]
    pub threshold_db: f64,
    /// The most it turns the band down (dB, negative).
    #[serde(default = "d_deess_range")]
    pub range_db: f64,
}

impl Default for DeEss {
    fn default() -> Self {
        DeEss { hz: d_deess_hz(), threshold_db: d_deess_threshold(), range_db: d_deess_range() }
    }
}

fn d_deess_hz() -> f64 {
    6000.0
}
fn d_deess_threshold() -> f64 {
    -6.0
}
fn d_deess_range() -> f64 {
    -6.0
}

impl DeEss {
    /// The dynamic band it runs as.
    pub fn band(&self) -> DynBand {
        DynBand {
            kind: EqKind::HighShelf,
            hz: self.hz,
            q: 0.7,
            threshold_db: self.threshold_db,
            relative: true,
            ratio: 4.0,
            range_db: self.range_db,
            attack_ms: 1.0,
            release_ms: 60.0,
        }
    }
}

fn d_attack() -> f64 {
    15.0
}
fn d_release() -> f64 {
    150.0
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Gate {
    pub threshold_db: f64,
    #[serde(default = "d_gate_att")]
    pub attack_ms: f64,
    #[serde(default = "d_gate_rel")]
    pub release_ms: f64,
}

fn d_gate_att() -> f64 {
    0.5
}
fn d_gate_rel() -> f64 {
    30.0
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct DelayFx {
    pub seconds: f64,
    pub feedback: f64,
    pub mix: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Duck {
    /// The track whose note-ons trigger the dip (e.g. the kick drum track).
    pub source_track: u64,
    pub depth_db: f64,
    pub release_ms: f64,
}

/// Modulated-delay chorus (JUCE/pedalboard law: delay = centre + 10 ms * depth * lfo).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChorusFx {
    /// LFO rate; in looped projects it snaps to a whole number of cycles per loop so the seam stays clean.
    pub rate_hz: f64,
    /// 0..1
    pub depth: f64,
    pub mix: f64,
    #[serde(default = "d_chorus_centre")]
    pub centre_delay_ms: f64,
}
fn d_chorus_centre() -> f64 {
    7.0
}

/// Guitar-amp style distortion: tight pre-EQ, two oversampled saturation stages, then a cabinet-like
/// post filter. For high-gain guitars and gritty bass; `drive_db` is the plain tanh alternative.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AmpFx {
    /// Pre-gain into the shaper (dB). ~15 crunch, ~30 high gain, 40+ extreme.
    pub gain_db: f64,
    /// Pre high-pass that keeps palm mutes tight (Hz).
    pub tight_hz: f64,
    /// Pre mid push, screamer style (centre Hz and dB).
    pub mid_hz: f64,
    pub mid_db: f64,
    /// Cabinet: low resonance / high-pass and high roll-off (Hz).
    pub cab_low_hz: f64,
    pub cab_high_hz: f64,
    /// Post cut around 400-500 Hz (dB, negative scoops).
    pub scoop_db: f64,
    /// Post presence peak around 2.8 kHz (dB).
    pub presence_db: f64,
    /// Output level (dB) after the cabinet.
    pub level_db: f64,
}

impl Default for AmpFx {
    fn default() -> Self {
        AmpFx {
            gain_db: 30.0,
            tight_hz: 130.0,
            mid_hz: 800.0,
            mid_db: 6.0,
            cab_low_hz: 75.0,
            cab_high_hz: 5200.0,
            scoop_db: -4.0,
            presence_db: 2.0,
            level_db: -10.0,
        }
    }
}

/// Where a track's insert sits in its channel strip.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InsertPosition {
    /// Straight after the instrument, before the filters and EQ (an amp simulator, say).
    Pre,
    /// After the strip's dynamics and delay, before the duck, panning and sends (a reverb, a lo-fi effect).
    #[default]
    Post,
}

impl InsertPosition {
    fn is_post(&self) -> bool {
        *self == InsertPosition::Post
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A VST3 effect in an insert chain: a track's, the master bus's or the reverb return's.
///
/// Before each render the plugin is set to `preset` (or its default state), then `params` are applied, as for
/// VST3 instruments.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Insert {
    /// Unique in the project (from `Project::next_id`, shared with tracks), so it survives reordering.
    pub id: u64,
    /// The `.vst3` bundle.
    pub path: String,
    /// Which class of a multi-plugin bundle (name or class id); None = the first effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Plugin parameters by name, as for a VST3 instrument's `params`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, serde_json::Value>,
    /// A `.vstpreset` with the plugin's full state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// What the preset is (the vendor preset or factory program it came from), for people reading the project.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preset_name: String,
    /// Passes the audio through untouched (the plugin stays loaded).
    #[serde(default, skip_serializing_if = "is_false")]
    pub bypass: bool,
    /// Tracks only; the master's and the reverb return's chains each have one place.
    #[serde(default, skip_serializing_if = "InsertPosition::is_post")]
    pub position: InsertPosition,
}

impl Insert {
    /// The plugin's name as its file names it ("ValhallaSupermassive").
    pub fn plugin_name(&self) -> &str {
        plugin_name(&self.path)
    }
}

/// A plugin's name from its `.vst3` path.
pub fn plugin_name(path: &str) -> &str {
    let path = path.trim_end_matches(['/', '\\']);
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.strip_suffix(".vst3").or_else(|| file.strip_suffix(".VST3")).unwrap_or(file)
}

/// Which insert chain: a track's, the master bus's, or the reverb return's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Chain {
    Track(u64),
    Master,
    Reverb,
}

/// The slopes a channel strip's high- and low-pass come in, dB per octave.
pub const FILTER_SLOPES: [u32; 4] = [12, 24, 36, 48];

/// Per-track channel strip, applied in this order:
/// pre inserts -> HPF/LPF -> EQ -> gain -> drive -> amp -> chorus -> bitcrush -> clip -> post gain -> gate -> compressor
/// -> dynamic EQ -> de-esser -> delay -> reverb -> post inserts -> duck -> mono below -> width/pan -> sends and output.
///
/// A track whose output another track routes to (`output`), or that another sends to (`sends`), is a bus: its strip
/// runs on its own sound (if it has an instrument) plus everything routed and sent to it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Mixer {
    pub gain_db: f64,
    pub pan: f64,
    pub width: f64,
    pub mute: bool,
    pub solo: bool,
    pub hpf_hz: f64,
    pub lpf_hz: f64,
    /// Slopes of the high- and low-pass, dB per octave: one of [`FILTER_SLOPES`] (Butterworth).
    pub hpf_slope: u32,
    pub lpf_slope: u32,
    /// Below this the track is mono (its side signal high-passed at 24 dB/octave, applied with the width); 0 = off.
    pub mono_below_hz: f64,
    pub eq: Vec<EqBand>,
    pub drive_db: f64,
    pub amp: Option<AmpFx>,
    pub chorus: Option<ChorusFx>,
    /// Quantize to this many bits (0 = off).
    pub bitcrush_bits: f64,
    /// Hard clip at this level in dBFS (None = off).
    pub clip_db: Option<f64>,
    pub post_gain_db: f64,
    pub gate: Option<Gate>,
    pub comp: Option<Comp>,
    pub delay: Option<DelayFx>,
    pub duck: Option<Duck>,
    pub reverb_send: f64,
    /// VST3 effects, in processing order (each at its `position`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inserts: Vec<Insert>,
    /// Dynamic EQ bands, after the compressor.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dyn_eq: Vec<DynBand>,
    /// After the dynamic EQ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deess: Option<DeEss>,
    /// A built-in reverb after the delay. On an aux return, `mix` 1 makes the strip return only the reverb.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverb: Option<ReverbFx>,
    /// The track (a group bus) this one's output goes to instead of the master.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// Post-fader sends to other tracks (aux returns), besides `reverb_send` to the master's reverb.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<AuxSend>,
}

/// A post-fader send from a track to another track (an aux return).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct AuxSend {
    /// The receiving track's id.
    pub to: u64,
    /// 0..1, as `reverb_send`.
    pub level: f64,
}

/// A reverb inside a channel strip.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReverbFx {
    #[serde(flatten)]
    pub params: ReverbParams,
    /// The reverb's share of the output: 1 = only the reverb (an aux return), 0.2 = mostly the dry signal.
    #[serde(default = "d_one")]
    pub mix: f64,
}

impl Default for ReverbFx {
    fn default() -> Self {
        ReverbFx { params: ReverbParams::default(), mix: 1.0 }
    }
}

impl Default for Mixer {
    fn default() -> Self {
        Mixer {
            gain_db: 0.0,
            pan: 0.0,
            width: 1.0,
            mute: false,
            solo: false,
            hpf_hz: 0.0,
            lpf_hz: 0.0,
            hpf_slope: 12,
            lpf_slope: 12,
            mono_below_hz: 0.0,
            eq: vec![],
            drive_db: 0.0,
            amp: None,
            chorus: None,
            bitcrush_bits: 0.0,
            clip_db: None,
            post_gain_db: 0.0,
            gate: None,
            comp: None,
            delay: None,
            duck: None,
            reverb_send: 0.0,
            inserts: vec![],
            dyn_eq: vec![],
            deess: None,
            reverb: None,
            output: None,
            sends: vec![],
        }
    }
}

/// The built-in reverb's settings. Any left out take their defaults, except `algorithm`: left out, it's the classic
/// reverb, as in older projects.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReverbParams {
    /// 0..1: how big the space is (sets the decay time unless `decay_s` does).
    pub room_size: f64,
    /// 0..1: how much faster the highs die away than the lows.
    pub damping: f64,
    pub predelay_ms: f64,
    pub hpf_hz: f64,
    pub lpf_hz: f64,
    pub return_db: f64,
    /// Which reverb. Left out (older projects), the classic one.
    #[serde(default, skip_serializing_if = "ReverbAlgorithm::is_classic")]
    pub algorithm: ReverbAlgorithm,
    /// Decay time (RT60, seconds) in place of the one `room_size` gives; 0 = from `room_size`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub decay_s: f64,
    /// Stereo width of the reverb (0 = mono, 1 = full).
    #[serde(default = "d_one", skip_serializing_if = "is_one")]
    pub width: f64,
}

impl Default for ReverbParams {
    fn default() -> Self {
        ReverbParams {
            room_size: 0.7,
            damping: 0.5,
            predelay_ms: 20.0,
            hpf_hz: 220.0,
            lpf_hz: 7500.0,
            return_db: 0.0,
            algorithm: ReverbAlgorithm::Hall,
            decay_s: 0.0,
            width: 1.0,
        }
    }
}

/// The built-in reverb's algorithms.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReverbAlgorithm {
    /// The Freeverb-style reverb of older projects (mono in, metallic on long tails).
    #[default]
    Classic,
    /// A small, dense room: early reflections and a short tail.
    Room,
    /// A large hall: slow build, smooth long tail.
    Hall,
    /// A bright, dense plate with no early reflections.
    Plate,
}

impl ReverbAlgorithm {
    fn is_classic(&self) -> bool {
        *self == ReverbAlgorithm::Classic
    }
}

/// The master's final limiter, after the loudness drive: a fast soft-clipping stage takes the sharpest peaks, then a
/// lookahead stage holds the true peak under the ceiling. Older projects, which leave it out, get the settings the
/// limiter had then (6 ms, no release, no clipping stage).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Limiter {
    /// How far ahead it starts turning down for a peak (ms).
    pub lookahead_ms: f64,
    /// How slowly the gain recovers after limiting (ms); 0 = as fast as the lookahead.
    pub release_ms: f64,
    /// Program-dependent release: short peaks recover fast, sustained limiting at `release_ms`.
    pub auto_release: bool,
    /// The most the soft-clipping stage takes off the sharpest peaks before the lookahead stage (dB); 0 = off.
    pub clip_db: f64,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter { lookahead_ms: 5.0, release_ms: 80.0, auto_release: true, clip_db: 1.0 }
    }
}

impl Limiter {
    /// The limiter of projects saved before it had settings.
    pub fn classic() -> Limiter {
        Limiter { lookahead_ms: 6.0, release_ms: 0.0, auto_release: false, clip_db: 0.0 }
    }
}

/// A multiband compressor: the master split at `crossovers_hz` into bands (which sum back flat), each compressed
/// on its own.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Multiband {
    /// 1 to 3 crossover frequencies (Hz), ascending: 2 to 4 bands.
    pub crossovers_hz: Vec<f64>,
    /// One per band, low to high.
    pub bands: Vec<MbBand>,
}

impl Default for Multiband {
    fn default() -> Self {
        let band = |threshold_db, ratio, attack_ms, release_ms| MbBand { threshold_db, ratio, attack_ms, release_ms, gain_db: 0.0 };
        Multiband {
            crossovers_hz: vec![150.0, 2500.0, 8000.0],
            bands: vec![band(-4.0, 2.0, 30.0, 200.0), band(-3.0, 1.6, 20.0, 150.0), band(-3.0, 1.8, 10.0, 100.0), band(-4.0, 2.0, 5.0, 80.0)],
        }
    }
}

/// One band of the multiband compressor.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct MbBand {
    /// dB relative to the band's loud level (95th percentile of its short-term RMS).
    pub threshold_db: f64,
    pub ratio: f64,
    #[serde(default = "d_attack")]
    pub attack_ms: f64,
    #[serde(default = "d_release")]
    pub release_ms: f64,
    /// The band's level after it (dB).
    #[serde(default)]
    pub gain_db: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Master {
    pub glue: Option<Comp>,
    pub eq: Vec<EqBand>,
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
    pub reverb: ReverbParams,
    /// One-shot exports get short linear fades; loops never do (it would dip every loop).
    pub edge_fade_ms: f64,
    pub tail_s: f64,
    /// VST3 effects on the master bus: after the reverb return joins the tracks, before the glue compressor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inserts: Vec<Insert>,
    /// VST3 effects on the reverb return. With any, the tracks' reverb sends run through them instead of the
    /// built-in reverb (of whose params only `return_db` still applies), so a plugin reverb set fully wet
    /// becomes the song's shared reverb.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverb_inserts: Vec<Insert>,
    /// Parameters of the master's and the reverb return's inserts over time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation: Vec<crate::automation::Lane>,
    /// The final limiter (see [`Limiter`]).
    #[serde(default = "Limiter::classic")]
    pub limiter: Limiter,
    /// After the master EQ; None = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiband: Option<Multiband>,
    /// Below this the master is mono (Hz), after the multiband; 0 = off.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mono_below_hz: f64,
    /// The master's stereo width (0 = mono, 1 = as mixed, up to 2), with `mono_below_hz`.
    #[serde(default = "d_one", skip_serializing_if = "is_one")]
    pub width: f64,
}

impl Default for Master {
    fn default() -> Self {
        Master {
            glue: Some(Comp { threshold_db: -3.0, relative: true, ratio: 1.8, attack_ms: 30.0, release_ms: 250.0, ..Comp::default() }),
            eq: vec![],
            target_lufs: -15.0,
            ceiling_dbtp: -1.0,
            reverb: ReverbParams::default(),
            edge_fade_ms: 6.0,
            tail_s: 6.0,
            inserts: vec![],
            reverb_inserts: vec![],
            automation: vec![],
            limiter: Limiter::default(),
            multiband: None,
            mono_below_hz: 0.0,
            width: 1.0,
        }
    }
}

impl Project {
    pub fn new(name: &str, bpm: f64, bars: u32) -> Project {
        Project {
            name: name.to_string(),
            sample_rate: 44100,
            tempo: vec![TempoPoint { bar: 1, bpm }],
            time_signature: (4, 4),
            key: String::new(),
            bars,
            looped: true,
            sections: vec![],
            harmony: vec![],
            tracks: vec![],
            master: Master::default(),
            next_id: 1,
            notes: String::new(),
            comments: vec![],
        }
    }

    pub fn beats_per_bar(&self) -> f64 {
        self.time_signature.0 as f64 * 4.0 / self.time_signature.1 as f64
    }

    pub fn total_beats(&self) -> f64 {
        self.bars as f64 * self.beats_per_bar()
    }

    pub fn track_index(&self, r: &TrackRef) -> Option<usize> {
        match r {
            TrackRef::Id(id) => self.tracks.iter().position(|t| t.id == *id),
            TrackRef::Name(n) => self
                .tracks
                .iter()
                .position(|t| t.name == *n)
                .or_else(|| self.tracks.iter().position(|t| t.name.eq_ignore_ascii_case(n))),
        }
    }

    pub fn section_at(&self, bar: u32) -> Option<&Section> {
        self.sections.iter().rfind(|s| s.start_bar <= bar && bar < s.start_bar + s.bars)
    }

    /// An insert chain; None for a track that doesn't exist.
    pub fn chain(&self, c: Chain) -> Option<&Vec<Insert>> {
        match c {
            Chain::Track(id) => self.tracks.iter().find(|t| t.id == id).map(|t| &t.mixer.inserts),
            Chain::Master => Some(&self.master.inserts),
            Chain::Reverb => Some(&self.master.reverb_inserts),
        }
    }

    pub fn chain_mut(&mut self, c: Chain) -> Option<&mut Vec<Insert>> {
        match c {
            Chain::Track(id) => self.tracks.iter_mut().find(|t| t.id == id).map(|t| &mut t.mixer.inserts),
            Chain::Master => Some(&mut self.master.inserts),
            Chain::Reverb => Some(&mut self.master.reverb_inserts),
        }
    }

    /// Every insert in the project with the chain it's in: the tracks' in track order, then the reverb return's,
    /// then the master's.
    pub fn inserts(&self) -> impl Iterator<Item = (Chain, &Insert)> {
        let tracks = self.tracks.iter().flat_map(|t| t.mixer.inserts.iter().map(move |i| (Chain::Track(t.id), i)));
        let reverb = self.master.reverb_inserts.iter().map(|i| (Chain::Reverb, i));
        tracks.chain(reverb).chain(self.master.inserts.iter().map(|i| (Chain::Master, i)))
    }

    /// The insert with this id, the chain it's in and its index there.
    pub fn insert(&self, id: u64) -> Option<(Chain, usize, &Insert)> {
        let (c, ins) = self.inserts().find(|(_, i)| i.id == id)?;
        let at = self.chain(c)?.iter().position(|i| i.id == id)?;
        Some((c, at, ins))
    }

    pub fn insert_mut(&mut self, id: u64) -> Option<&mut Insert> {
        let (c, at, _) = self.insert(id)?;
        self.chain_mut(c)?.get_mut(at)
    }

    /// Takes an id for a new track, insert or clip. Ids are never reused, even after a delete.
    pub fn alloc_id(&mut self) -> u64 {
        // an older project may have ids at or past next_id (hand-edited files)
        let clips = self.tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.id));
        let used = self.tracks.iter().map(|t| t.id).chain(self.inserts().map(|(_, i)| i.id)).chain(clips).max().unwrap_or(0);
        let id = self.next_id.max(used + 1);
        self.next_id = id + 1;
        id
    }

    /// The clip with this id, with the index of its track and its index there.
    pub fn clip(&self, id: u64) -> Option<(usize, usize, &Clip)> {
        self.tracks.iter().enumerate().find_map(|(ti, t)| t.clips.iter().position(|c| c.id == id).map(|ci| (ti, ci, &t.clips[ci])))
    }
}

/// Reference to a track by numeric id or by name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TrackRef {
    Id(u64),
    Name(String),
}

impl std::fmt::Display for TrackRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackRef::Id(i) => write!(f, "#{i}"),
            TrackRef::Name(n) => write!(f, "{n}"),
        }
    }
}

/// Track colours: soft pastels that read on a light background (and still work on a dark one).
pub const PALETTE: [[u8; 3]; 12] = [
    [146, 178, 214], // sky
    [222, 166, 140], // peach
    [168, 196, 146], // sage
    [196, 164, 212], // lavender
    [224, 198, 130], // butter
    [134, 190, 176], // mint
    [218, 150, 158], // rose
    [170, 168, 222], // periwinkle
    [206, 176, 140], // sand
    [214, 160, 196], // mauve
    [186, 188, 130], // olive
    [196, 150, 122], // clay
];
