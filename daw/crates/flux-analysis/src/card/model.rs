//! The report card: one JSON file per instrument (`catalog/<slug>.json`).
//!
//! Field names, nesting and key order are those of `tools/instrument_card.py`, so existing cards, the catalog
//! README and anything reading them keep working. Additions (all optional, only written in
//! [`Schema::Extended`](super::Schema::Extended) mode) are marked "additive" below; see the crate README.

use crate::json::OrderedMap;
use serde::{Deserialize, Deserializer, Serialize};

fn some_option<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Card {
    pub name: String,
    pub slug: String,
    /// The SFZ path relative to the project root (forward slashes).
    pub sfz: String,
    /// The SFZ's header comment.
    #[serde(default)]
    pub about: Option<String>,
    /// Date measured (`YYYY-MM-DD`, UTC unless the caller sets it).
    #[serde(default)]
    pub measured: String,
    /// Additive: the program that wrote the card (`flux-analysis <version>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Conditions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<License>,
    /// Sections that failed, with the reason.
    #[serde(default)]
    pub errors: OrderedMap<String>,
    /// Set when the whole card failed (load failed, nothing sounds, crash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<KeyMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regions: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc_labels: Option<OrderedMap<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_labels: Option<OrderedMap<String>>,
    /// `Some(None)` is written as `null` (measured: the SFZ has no keyswitches).
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "some_option")]
    pub keyswitches: Option<Option<Keyswitches>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_key: Option<OrderedMap<KeyDetail>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<Pitch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timbre: Option<TimbreSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sustain: Option<Sustain>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity: Option<Velocity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc: Option<OrderedMap<CcResponse>>,
    /// Additive: the test note repeated, to expose round robins (or their absence) and random variation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_robin: Option<RoundRobin>,
    /// Additive: level and brightness over the whole velocity range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_curve: Option<VelocityCurve>,
    /// Additive: level over the whole range of CC1 (mod wheel) and CC11 (expression), keyed by CC number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc_curves: Option<OrderedMap<CcCurve>>,
    #[serde(default)]
    pub flags: OrderedMap<String>,
    /// Where the instrument is used (from `--used` runs; carried over from the previous card otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_by: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    /// Wall time spent on this card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Conditions {
    pub sr: u32,
    /// CC values sent before every note (`{"1": 70, "7": 100, "11": 127}`).
    pub cc: OrderedMap<i64>,
    /// Keyswitch pressed before every note.
    pub keyswitch: Option<i64>,
    /// Scanned key range.
    pub keys: [i64; 2],
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct License {
    /// The libraries' licenses joined with " + " ("unknown" when none was found).
    pub name: String,
    pub libraries: Vec<LibLicense>,
    /// Every library is listed in LICENSES.md with a matching license.
    pub all_in_licenses_md: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LibLicense {
    /// Folder under `instruments/`.
    pub library: String,
    pub license: String,
    /// The file the license was read from (relative to the root).
    pub file: Option<String>,
    /// The license LICENSES.md gives ("not listed" when missing).
    pub licenses_md: String,
    pub agrees: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Keyswitches {
    /// Keyswitch key -> label (empty when unlabelled).
    pub keys: OrderedMap<String>,
    pub default: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KeyMap {
    /// The longest run of sounding keys (bridging holes of 1-2 keys).
    pub main: [i64; 2],
    pub main_names: String,
    pub ranges: Vec<[i64; 2]>,
    pub count: i64,
    pub span: [i64; 2],
    /// Silent keys inside the main range.
    pub gaps: Vec<i64>,
    pub other_ranges: Vec<[i64; 2]>,
    pub isolated: Vec<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KeyDetail {
    /// RMS of the first 0.6 s at velocity 90.
    pub level_db: f64,
    /// YIN aperiodicity (0 = clearly pitched, 1 = noise).
    pub ap: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f0: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cents: Option<f64>,
    /// Additive: pitch at 0.3-0.5 s minus pitch at 0.04-0.2 s, in cents (positive = slides up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_cents: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Pitch {
    /// "pitched", "mixed" or "unpitched".
    pub kind: String,
    pub confident_keys: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_share: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_outliers: Option<OrderedMap<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offsets: Option<OrderedMap<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worst_cents: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worst_key: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_abs_cents: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_of_tune: Option<OrderedMap<f64>>,
    /// Additive: keys whose pitch moves by more than the slide threshold during the note (key -> cents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slides: Option<OrderedMap<f64>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Level {
    pub median_db: f64,
    pub std_db: f64,
    pub min_db: f64,
    pub max_db: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_jump_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_jump_keys: Option<[i64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jumps_over_6db: Option<i64>,
    /// Additive: keys much quieter than their neighbours on both sides (key -> dB below them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holes: Option<OrderedMap<f64>>,
}

/// Brightness and attack of one note (definitions in the module docs of [`super::measure`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Timbre {
    pub hi3k_db: f64,
    pub centroid_hz: f64,
    pub attack_db: f64,
    pub onset_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TimbreNote {
    pub key: i64,
    #[serde(flatten)]
    pub timbre: Timbre,
}

/// Medians of [`Timbre`] over a set of notes (`None` = no note had the value).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TimbreMedian {
    pub hi3k_db: Option<f64>,
    pub centroid_hz: Option<f64>,
    pub attack_db: Option<f64>,
    pub onset_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TimbreSection {
    pub velocity: i64,
    pub main_keys: [i64; 2],
    pub main: Option<TimbreMedian>,
    pub median: Option<TimbreMedian>,
    /// Octave ("C4") -> medians of the notes in it.
    pub octaves: OrderedMap<TimbreMedian>,
    pub notes: Vec<TimbreNote>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Sustain {
    pub key: i64,
    /// "sustained", "sustained, sample ends" or "decaying".
    pub kind: String,
    pub peak_at_s: f64,
    pub t40_s: Option<f64>,
    pub level_at_2s_db: f64,
    pub usable_max_note_s: Option<f64>,
    pub release_s: Option<f64>,
    pub note: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VelocityLevel {
    pub rms_db: f64,
    #[serde(flatten)]
    pub timbre: Timbre,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Velocity {
    pub key: i64,
    /// "30", "70", "110" -> level and timbre.
    pub levels: OrderedMap<VelocityLevel>,
    pub range_db: f64,
    pub hi3k_change_db: f64,
    pub kind: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CcResponse {
    pub level_change_db: Option<f64>,
    pub hi3k_change_db: Option<f64>,
    pub silent_at_0: bool,
    pub changes_level: bool,
    pub changes_timbre: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RoundRobin {
    pub key: i64,
    pub velocity: i64,
    /// Level of each repetition (first 0.6 s).
    pub levels_db: Vec<f64>,
    pub level_spread_db: f64,
    pub onsets_ms: Vec<Option<f64>>,
    pub onset_spread_ms: Option<f64>,
    /// How many different recordings the repetitions played (waveform correlation below 0.999 = different).
    pub distinct: i64,
    /// Repeats after this many notes (round robin length), when it cycles.
    pub cycle: Option<i64>,
    /// The SFZ draws random numbers (lorand/hirand, *_random), so repetitions vary by chance.
    pub random: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VelocityCurve {
    pub key: i64,
    pub velocities: Vec<i64>,
    pub rms_db: Vec<f64>,
    pub hi3k_db: Vec<Option<f64>>,
    /// Louder at every higher velocity (within 0.5 dB).
    pub monotonic: bool,
    pub range_db: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CcCurve {
    pub key: i64,
    pub values: Vec<i64>,
    pub rms_db: Vec<f64>,
    pub hi3k_db: Vec<Option<f64>>,
    /// CC values at which the note is silent (below -90 dB or 40 dB under the loudest value).
    pub silent_at: Vec<i64>,
    /// The note goes silent at a value above one where it sounds (a dead zone in the CC response).
    pub dropout: bool,
}
