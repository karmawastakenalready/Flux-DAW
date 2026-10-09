//! The rig file format (`*.rig.json`): an SFZ source and an insert chain, with validation and job-parameter
//! overrides.
//!
//! ```json
//! {"about": "...",
//!  "source": {"sfz": "instruments/.../program.sfz", "init_cc": {"22": 51}, "keyswitches": [[0, 29], [89, 127]],
//!             "tune_cents": -5, "rr_offset": 0, "default_ks": 17},
//!  "chain": [{"type": "highpass", "hz": 70},
//!            {"type": "nam", "model": "instruments/nam/.../x.nam", "input_db": 0, "output_db": 0},
//!            {"type": "ir", "path": "...wav", "mix": 1.0},
//!            {"type": "vst3", "path": "...vst3", "params": {}},
//!            {"type": "gate", "threshold_db": -55, "release_ms": 80},
//!            {"type": "peak", "hz": 2500, "db": -2, "q": 1.0}, {"type": "lowpass", "hz": 9000},
//!            {"id": "out", "type": "gain", "db": -6}]}
//! ```
//!
//! Paths are relative to a root directory (the repository root for the rigs in `sfz/rigs`). Every stage may carry
//! an `id`; job parameters such as `{"nam.input_db": 3}` override a field of the first stage whose `id`, or else
//! whose `type`, matches the part before the first dot (see [`Rig::with_overrides`]). This is the format and the
//! semantics of `daw/bridge/rig.py`; unlike the Python tool, unknown fields and out-of-range values are errors.

use crate::error::{Error, Result};
use crate::json::{Domain, Obj, as_int, describe, init_cc_order};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// A loaded, validated rig.
#[derive(Clone, Debug)]
pub struct Rig {
    /// Where the rig came from (file path or a caller-supplied label), used in error messages.
    pub label: String,
    /// Directory that relative paths in the rig resolve against.
    pub root: PathBuf,
    /// Free-text description.
    pub about: Option<String>,
    /// The instrument that feeds the chain.
    pub source: Source,
    /// Insert stages, in processing order.
    pub chain: Vec<Stage>,
    /// The chain as JSON, kept so overrides re-run the same validation as the file.
    raw_chain: Vec<Value>,
    /// Whether referenced files were checked to exist (and so must be after overrides).
    files_checked: bool,
}

/// The SFZ instrument a rig plays.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    /// The SFZ file as written in the rig.
    pub sfz_raw: String,
    /// The SFZ file resolved against the rig root.
    pub sfz: PathBuf,
    /// Controllers sent at time 0, in file order: (controller, value).
    pub init_cc: Vec<(u8, u8)>,
    /// Inclusive key ranges that are keyswitches rather than notes.
    pub keyswitches: Vec<(u8, u8)>,
    /// Detune in cents (sent to sfizz as a tuning frequency, see [`Source::tuning_frequency`]).
    pub tune_cents: f64,
    /// Round-robin offset: how many notes every round-robin counter is advanced before the song starts, so a
    /// double track on the same rig picks different samples than its twin (see [`crate::events`]).
    pub rr_offset: u32,
    /// Keyswitch to leave selected after the round-robin pre-roll.
    pub default_ks: Option<u8>,
}

impl Source {
    /// Whether `pitch` falls in one of the keyswitch ranges.
    pub fn is_keyswitch(&self, pitch: u8) -> bool {
        self.keyswitches.iter().any(|&(lo, hi)| lo <= pitch && pitch <= hi)
    }

    /// The A4 frequency to give sfizz (`sfizz_set_tuning_frequency`), or `None` when the rig is not detuned.
    pub fn tuning_frequency(&self) -> Option<f64> {
        (self.tune_cents != 0.0).then(|| 440.0 * 2f64.powf(self.tune_cents / 1200.0))
    }
}

/// One insert stage.
#[derive(Clone, Debug, PartialEq)]
pub struct Stage {
    /// Optional name that parameter overrides can address.
    pub id: Option<String>,
    /// What the stage does.
    pub kind: StageKind,
}

impl Stage {
    /// The stage's `type` as written in the rig file.
    pub fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }
}

/// The processing of one stage, with every default filled in.
#[derive(Clone, Debug, PartialEq)]
pub enum StageKind {
    /// `highpass`, `lowpass`, `peak`, `lowshelf`, `highshelf`.
    Filter(FilterSpec),
    /// `ir`: cabinet / room impulse response.
    Ir(IrSpec),
    /// `gate`: noise gate.
    Gate(DynamicsSpec),
    /// `comp`: compressor.
    Comp(DynamicsSpec),
    /// `gain`: fixed gain.
    Gain { db: f64 },
    /// `nam`: Neural Amp Modeler capture.
    Nam(NamSpec),
    /// `vst3`: a plugin, hosted by the engine (see [`crate::StageHooks`]).
    Vst3(Vst3Spec),
}

impl StageKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            StageKind::Filter(f) => f.shape.type_name(),
            StageKind::Ir(_) => "ir",
            StageKind::Gate(_) => "gate",
            StageKind::Comp(_) => "comp",
            StageKind::Gain { .. } => "gain",
            StageKind::Nam(_) => "nam",
            StageKind::Vst3(_) => "vst3",
        }
    }
}

/// Filter shapes (pedalboard / JUCE semantics: first-order high/low pass, RBJ peak and shelves).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterShape {
    Highpass,
    Lowpass,
    Peak,
    LowShelf,
    HighShelf,
}

impl FilterShape {
    pub fn type_name(self) -> &'static str {
        match self {
            FilterShape::Highpass => "highpass",
            FilterShape::Lowpass => "lowpass",
            FilterShape::Peak => "peak",
            FilterShape::LowShelf => "lowshelf",
            FilterShape::HighShelf => "highshelf",
        }
    }
}

/// A filter stage. `gain_db` (within -100 exclusive ..= 100 dB) and `q` are unused by the high/low pass shapes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterSpec {
    pub shape: FilterShape,
    pub hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

/// An impulse-response stage.
#[derive(Clone, Debug, PartialEq)]
pub struct IrSpec {
    pub path_raw: String,
    pub path: PathBuf,
    /// Wet proportion, 0..=1 (linear dry/wet).
    pub mix: f64,
}

/// Gate and compressor settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicsSpec {
    pub threshold_db: f64,
    pub ratio: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
}

/// A NAM capture stage.
#[derive(Clone, Debug, PartialEq)]
pub struct NamSpec {
    pub model_raw: String,
    pub model: PathBuf,
    pub input_db: f64,
    pub output_db: f64,
    /// For A2 (`SlimmableContainer`) files: use the smallest submodel whose `max_value` is at least this
    /// (0 < size <= 1). `None` uses the largest submodel, as `rig.py` does.
    pub size: Option<f64>,
}

/// A VST3 plugin stage.
#[derive(Clone, Debug, PartialEq)]
pub struct Vst3Spec {
    pub path_raw: String,
    pub path: PathBuf,
    /// Plugin parameters by name, as in the rig file (numbers, or text for choice parameters).
    pub params: Map<String, Value>,
}

impl Rig {
    /// Load a rig file. Relative paths inside it resolve against `root`; referenced files must exist.
    pub fn load(path: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<Rig> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let mut rig = Rig::from_json_str(&text, root, path.display().to_string()).map_err(|e| match e {
            Error::Json { source, .. } => Error::Json { path: path.to_path_buf(), source },
            other => other,
        })?;
        rig.check_files()?;
        Ok(rig)
    }

    /// Parse rig JSON text without touching the file system. `label` names it in error messages.
    pub fn from_json_str(text: &str, root: impl AsRef<Path>, label: impl Into<String>) -> Result<Rig> {
        let label = label.into();
        let v: Value =
            serde_json::from_str(text).map_err(|source| Error::Json { path: PathBuf::from(&label), source })?;
        Rig::parse_with_order(&v, root.as_ref(), label, init_cc_order(text).as_deref())
    }

    /// Parse a rig from a JSON value without touching the file system. `label` names it in error messages.
    ///
    /// `serde_json` objects do not keep key order, so `source.init_cc` controllers are sent in ascending
    /// controller order here; [`Rig::from_json_str`] and [`Rig::load`] keep the file order, as `rig.py` does.
    pub fn parse(v: &Value, root: impl AsRef<Path>, label: impl Into<String>) -> Result<Rig> {
        Rig::parse_with_order(v, root.as_ref(), label.into(), None)
    }

    fn parse_with_order(v: &Value, root: &Path, label: String, cc_order: Option<&[String]>) -> Result<Rig> {
        let root = root.to_path_buf();
        let mut o = Obj::new(v, label.clone(), Domain::Rig)?;
        let about = o.opt_string("about")?.map(str::to_owned);
        let mut src = o.child("source")?;
        let source = parse_source(&mut src, &root, cc_order)?;
        src.finish("unknown field")?;
        let raw_chain: Vec<Value> = o.opt_array("chain")?.map(<[Value]>::to_vec).unwrap_or_default();
        o.finish("unknown field")?;
        let chain = parse_chain(&raw_chain, &root, &label)?;
        Ok(Rig { label, root, about, source, chain, raw_chain, files_checked: false })
    }

    /// Check that every file the rig references exists.
    pub fn check_files(&mut self) -> Result<()> {
        let mut need: Vec<(String, &Path)> = vec![(format!("{}: source.sfz", self.label), &self.source.sfz)];
        for (i, st) in self.chain.iter().enumerate() {
            let at = format!("{}: chain[{i}] ({})", self.label, st.type_name());
            match &st.kind {
                StageKind::Ir(s) => need.push((format!("{at}.path"), &s.path)),
                StageKind::Nam(s) => need.push((format!("{at}.model"), &s.model)),
                StageKind::Vst3(s) => need.push((format!("{at}.path"), &s.path)),
                _ => {}
            }
        }
        for (at, p) in need {
            if !p.exists() {
                return Err(Error::rig(at, format!("file not found: {}", p.display())));
            }
        }
        self.files_checked = true;
        Ok(())
    }

    /// The rig with job parameters applied, exactly as `rig.py` does: for each `"name.field": value`, the
    /// first stage whose `id` equals `name`, or failing that the first whose `type` does, gets `field` set to
    /// `value` (the split is at the first dot). The result is validated like a rig file, so an override of an
    /// unknown field or with a value of the wrong type is an error naming the parameter.
    pub fn with_overrides<'a, K, I>(&self, params: I) -> Result<Rig>
    where
        K: AsRef<str>,
        I: IntoIterator<Item = (K, &'a Value)>,
    {
        let mut chain = self.raw_chain.clone();
        let mut applied: Vec<String> = vec![];
        for (key, value) in params {
            let key = key.as_ref();
            let (name, field) = key.split_once('.').unwrap_or((key, ""));
            let by = |pred: &dyn Fn(&Map<String, Value>) -> bool| {
                chain.iter().position(|s| s.as_object().is_some_and(pred))
            };
            let idx = by(&|o| o.get("id").and_then(Value::as_str) == Some(name))
                .or_else(|| by(&|o| o.get("type").and_then(Value::as_str) == Some(name)));
            let Some(i) = idx.filter(|_| !field.is_empty()) else {
                return Err(Error::rig(format!("{}: rig param {key:?}", self.label), "matches no stage"));
            };
            if let Some(o) = chain[i].as_object_mut() {
                o.insert(field.to_owned(), value.clone());
            }
            applied.push(key.to_owned());
        }
        let parsed = parse_chain(&chain, &self.root, &self.label).map_err(|e| match e {
            Error::Rig { at, message } => {
                Error::Rig { at, message: format!("{message} (after rig params {})", applied.join(", ")) }
            }
            other => other,
        })?;
        let mut rig = Rig {
            label: self.label.clone(),
            root: self.root.clone(),
            about: self.about.clone(),
            source: self.source.clone(),
            chain: parsed,
            raw_chain: chain,
            files_checked: false,
        };
        if self.files_checked {
            rig.check_files()?;
        }
        Ok(rig)
    }

    /// The chain as JSON (with overrides applied), e.g. for display.
    pub fn chain_json(&self) -> &[Value] {
        &self.raw_chain
    }
}

fn midi(o: &Obj, key: &str, v: &Value) -> Result<u8> {
    match as_int(v) {
        Some(i @ 0..=127) => Ok(i as u8),
        _ => Err(o.field_error(key, format!("expected a MIDI number 0..=127, got {}", describe(v)))),
    }
}

fn parse_source(o: &mut Obj, root: &Path, cc_order: Option<&[String]>) -> Result<Source> {
    let sfz_raw = o.string("sfz")?.to_owned();
    if sfz_raw.trim().is_empty() {
        return Err(o.field_error("sfz", "must not be empty"));
    }
    let mut init_cc = vec![];
    if let Some(v) = o.get("init_cc") {
        let Value::Object(m) = v else {
            return Err(o.field_error("init_cc", format!("expected an object {{\"cc\": value}}, got {}", describe(v))));
        };
        let mut keyed = vec![];
        for (k, val) in m {
            let cc = k.trim().parse::<i64>().ok().filter(|c| (0..=127).contains(c)).ok_or_else(|| {
                o.field_error("init_cc", format!("controller {k:?} is not a number 0..=127"))
            })?;
            let value = midi(o, &format!("init_cc.{k}"), val)?;
            let pos = cc_order.and_then(|ord| ord.iter().position(|x| x == k)).unwrap_or(usize::MAX);
            keyed.push((pos, cc as u8, value));
        }
        // file order when known (as Python's dict keeps it), else ascending controller number
        keyed.sort_by_key(|&(pos, cc, _)| (pos, cc));
        init_cc = keyed.into_iter().map(|(_, cc, v)| (cc, v)).collect();
    }
    let mut keyswitches = vec![];
    for (i, r) in o.opt_array("keyswitches")?.unwrap_or_default().iter().enumerate() {
        let key = format!("keyswitches[{i}]");
        let pair = r.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
            o.field_error(&key, format!("expected a [low, high] pair, got {}", describe(r)))
        })?;
        let (lo, hi) = (midi(o, &key, &pair[0])?, midi(o, &key, &pair[1])?);
        if lo > hi {
            return Err(o.field_error(&key, format!("low key {lo} is above high key {hi}")));
        }
        keyswitches.push((lo, hi));
    }
    let tune_cents = o.num_or("tune_cents", 0.0)?;
    if tune_cents.abs() > 1200.0 {
        return Err(o.field_error("tune_cents", format!("{tune_cents} is outside -1200..=1200")));
    }
    let rr_offset = match o.opt_int("rr_offset")? {
        None => 0,
        Some(n @ 0..=1000) => n as u32,
        Some(n) => return Err(o.field_error("rr_offset", format!("{n} is outside 0..=1000"))),
    };
    let default_ks = match o.get("default_ks") {
        None => None,
        Some(v) => Some(midi(o, "default_ks", v)?),
    };
    Ok(Source { sfz: root.join(&sfz_raw), sfz_raw, init_cc, keyswitches, tune_cents, rr_offset, default_ks })
}

fn parse_chain(raw: &[Value], root: &Path, label: &str) -> Result<Vec<Stage>> {
    raw.iter().enumerate().map(|(i, v)| parse_stage(v, root, &format!("{label}: chain[{i}]"))).collect()
}

fn positive(o: &Obj, key: &str, v: f64) -> Result<f64> {
    if v > 0.0 { Ok(v) } else { Err(o.field_error(key, format!("must be above 0, got {v}"))) }
}

fn parse_stage(v: &Value, root: &Path, at: &str) -> Result<Stage> {
    let mut o = Obj::new(v, at, Domain::Rig)?;
    let ty = o.string("type")?;
    let id = match o.get("id") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => return Err(o.field_error("id", format!("expected a string, got {}", describe(other)))),
    };
    let mut o = Obj::new(v, format!("{at} ({ty})"), Domain::Rig)?;
    o.ignore(&["type", "id"]);
    let filter = |o: &mut Obj, shape: FilterShape, q_default: f64| -> Result<StageKind> {
        let hz = o.num("hz")?;
        let hz = positive(o, "hz", hz)?;
        let (gain_db, q) = match shape {
            // first order: no gain, no resonance
            FilterShape::Highpass | FilterShape::Lowpass => (0.0, q_default),
            _ => {
                let db = o.num("db")?;
                // JUCE treats -100 dB and below as silence (gain 0), which makes these designs divide by zero
                if !(db > -100.0 && db <= 100.0) {
                    return Err(o.field_error("db", format!("must be above -100 and at most 100 dB, got {db}")));
                }
                let q = o.num_or("q", q_default)?;
                (db, positive(o, "q", q)?)
            }
        };
        Ok(StageKind::Filter(FilterSpec { shape, hz, gain_db, q }))
    };
    let dynamics = |o: &mut Obj, d: DynamicsSpec| -> Result<DynamicsSpec> {
        let s = DynamicsSpec {
            threshold_db: o.num_or("threshold_db", d.threshold_db)?,
            ratio: o.num_or("ratio", d.ratio)?,
            attack_ms: o.num_or("attack_ms", d.attack_ms)?,
            release_ms: o.num_or("release_ms", d.release_ms)?,
        };
        if s.ratio < 1.0 {
            return Err(o.field_error("ratio", format!("must be at least 1, got {}", s.ratio)));
        }
        for (k, x) in [("attack_ms", s.attack_ms), ("release_ms", s.release_ms)] {
            if x < 0.0 {
                return Err(o.field_error(k, format!("must not be negative, got {x}")));
            }
        }
        Ok(s)
    };
    let path = |o: &mut Obj, key: &str| -> Result<(String, PathBuf)> {
        let raw = o.string(key)?.to_owned();
        if raw.trim().is_empty() {
            return Err(o.field_error(key, "must not be empty"));
        }
        let p = root.join(&raw);
        Ok((raw, p))
    };
    let kind = match ty {
        "highpass" => filter(&mut o, FilterShape::Highpass, std::f64::consts::FRAC_1_SQRT_2)?,
        "lowpass" => filter(&mut o, FilterShape::Lowpass, std::f64::consts::FRAC_1_SQRT_2)?,
        "peak" => filter(&mut o, FilterShape::Peak, 1.0)?,
        "lowshelf" => filter(&mut o, FilterShape::LowShelf, 0.7)?,
        "highshelf" => filter(&mut o, FilterShape::HighShelf, 0.7)?,
        "gain" => StageKind::Gain { db: o.num("db")? },
        "gate" => StageKind::Gate(dynamics(
            &mut o,
            DynamicsSpec { threshold_db: -55.0, ratio: 10.0, attack_ms: 1.0, release_ms: 80.0 },
        )?),
        "comp" => StageKind::Comp(dynamics(
            &mut o,
            DynamicsSpec { threshold_db: -18.0, ratio: 3.0, attack_ms: 10.0, release_ms: 120.0 },
        )?),
        "ir" => {
            let (path_raw, path) = path(&mut o, "path")?;
            let mix = o.num_or("mix", 1.0)?;
            if !(0.0..=1.0).contains(&mix) {
                return Err(o.field_error("mix", format!("must be within 0..=1, got {mix}")));
            }
            StageKind::Ir(IrSpec { path_raw, path, mix })
        }
        "nam" => {
            let (model_raw, model) = path(&mut o, "model")?;
            let input_db = o.num_or("input_db", 0.0)?;
            let output_db = o.num_or("output_db", 0.0)?;
            let size = o.opt_num("size")?;
            if let Some(s) = size
                && !(s > 0.0 && s <= 1.0)
            {
                return Err(o.field_error("size", format!("must be within (0, 1], got {s}")));
            }
            StageKind::Nam(NamSpec { model_raw, model, input_db, output_db, size })
        }
        "vst3" => {
            let (path_raw, path) = path(&mut o, "path")?;
            let params = match o.get("params") {
                None => Map::new(),
                Some(Value::Object(m)) => m.clone(),
                Some(other) => return Err(o.field_error("params", format!("expected an object, got {}", describe(other)))),
            };
            StageKind::Vst3(Vst3Spec { path_raw, path, params })
        }
        other => {
            return Err(Error::rig(
                format!("{at}.type"),
                format!(
                    "unknown rig stage {other:?} (expected highpass, lowpass, peak, lowshelf, highshelf, ir, gate, comp, gain, nam or vst3)"
                ),
            ));
        }
    };
    o.finish("unknown field")?;
    Ok(Stage { id, kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rig(chain: Value) -> Rig {
        Rig::parse(&json!({"source": {"sfz": "a.sfz"}, "chain": chain}), "/root", "test.rig.json").unwrap()
    }

    #[test]
    fn defaults_follow_rig_py() {
        let r = rig(json!([
            {"type": "peak", "hz": 2500, "db": -2},
            {"type": "lowshelf", "hz": 100, "db": 3},
            {"type": "gate"}, {"type": "comp"},
            {"type": "ir", "path": "cab.wav"},
            {"type": "nam", "model": "m.nam"},
        ]));
        let k: Vec<&StageKind> = r.chain.iter().map(|s| &s.kind).collect();
        assert!(matches!(k[0], StageKind::Filter(FilterSpec { q, .. }) if *q == 1.0));
        assert!(matches!(k[1], StageKind::Filter(FilterSpec { q, .. }) if *q == 0.7));
        assert_eq!(
            k[2],
            &StageKind::Gate(DynamicsSpec { threshold_db: -55.0, ratio: 10.0, attack_ms: 1.0, release_ms: 80.0 })
        );
        assert_eq!(
            k[3],
            &StageKind::Comp(DynamicsSpec { threshold_db: -18.0, ratio: 3.0, attack_ms: 10.0, release_ms: 120.0 })
        );
        assert!(matches!(k[4], StageKind::Ir(IrSpec { mix, .. }) if *mix == 1.0));
        assert!(matches!(k[5], StageKind::Nam(NamSpec { input_db, output_db, size: None, .. }) if *input_db == 0.0 && *output_db == 0.0));
        assert_eq!(r.source.tune_cents, 0.0);
        assert_eq!(r.source.rr_offset, 0);
        assert!(r.source.tuning_frequency().is_none());
    }

    #[test]
    fn init_cc_keeps_file_order() {
        let text = r#"{"source": {"sfz": "a.sfz", "init_cc": {"22": 51, "32": 0, "27": 8}}}"#;
        let r = Rig::from_json_str(text, "/", "t").unwrap();
        assert_eq!(r.source.init_cc, vec![(22, 51), (32, 0), (27, 8)]);
        // from a Value the order is lost: ascending controller numbers
        let r = Rig::parse(&serde_json::from_str(text).unwrap(), "/", "t").unwrap();
        assert_eq!(r.source.init_cc, vec![(22, 51), (27, 8), (32, 0)]);
        let e = Rig::from_json_str("{\"source\": ", "/", "t.rig.json").unwrap_err().to_string();
        assert!(e.starts_with("t.rig.json: invalid JSON"), "{e}");
    }

    #[test]
    fn overrides_match_id_first_then_type() {
        let r = rig(json!([
            {"type": "gain", "db": 0},
            {"id": "gain", "type": "nam", "model": "m.nam"},
            {"type": "nam", "model": "n.nam", "id": "second"},
        ]));
        // "gain" names the nam stage by id, not the gain stage by type
        let o = r.with_overrides([("gain.input_db", &json!(3))]).unwrap();
        assert!(matches!(&o.chain[1].kind, StageKind::Nam(n) if n.input_db == 3.0));
        assert!(matches!(&o.chain[0].kind, StageKind::Gain { db } if *db == 0.0));
        // "nam" by type: the first nam stage
        let o = r.with_overrides([("nam.output_db", &json!(-6.0))]).unwrap();
        assert!(matches!(&o.chain[1].kind, StageKind::Nam(n) if n.output_db == -6.0));
        assert!(matches!(&o.chain[2].kind, StageKind::Nam(n) if n.output_db == 0.0));
        // the split is at the first dot
        let o = r.with_overrides([("second.input_db", &json!(1.5))]).unwrap();
        assert!(matches!(&o.chain[2].kind, StageKind::Nam(n) if n.input_db == 1.5));
        // the original is untouched
        assert!(matches!(&r.chain[1].kind, StageKind::Nam(n) if n.input_db == 0.0));
    }

    #[test]
    fn override_errors_name_the_param() {
        let r = rig(json!([{"type": "nam", "model": "m.nam"}]));
        let e = r.with_overrides([("amp.input_db", &json!(3))]).unwrap_err().to_string();
        assert!(e.contains("\"amp.input_db\"") && e.contains("matches no stage"), "{e}");
        let e = r.with_overrides([("nam", &json!(3))]).unwrap_err().to_string();
        assert!(e.contains("matches no stage"), "{e}");
        let e = r.with_overrides([("nam.inptu_db", &json!(3))]).unwrap_err().to_string();
        assert!(e.contains("inptu_db") && e.contains("nam.inptu_db"), "{e}");
        let e = r.with_overrides([("nam.input_db", &json!("loud"))]).unwrap_err().to_string();
        assert!(e.contains("chain[0] (nam).input_db") && e.contains("\"loud\""), "{e}");
    }

    #[test]
    fn validation_messages_locate_the_problem() {
        let bad = |chain: Value| {
            Rig::parse(&json!({"source": {"sfz": "a.sfz"}, "chain": chain}), "/", "x.rig.json").unwrap_err().to_string()
        };
        let e = bad(json!([{"type": "highpass", "hz": -3}]));
        assert!(e.contains("x.rig.json: chain[0] (highpass).hz"), "{e}");
        let e = bad(json!([{"type": "peak", "hz": 300, "db": -100}]));
        assert!(e.contains("x.rig.json: chain[0] (peak).db") && e.contains("-100"), "{e}");
        let e = bad(json!([{"type": "lowshelf", "hz": 300, "db": 3, "q": 0}]));
        assert!(e.contains("chain[0] (lowshelf).q"), "{e}");
        let e = bad(json!([{"type": "lowpas", "hz": 3}]));
        assert!(e.contains("chain[0].type") && e.contains("unknown rig stage"), "{e}");
        let e = bad(json!([{"type": "gate", "treshold_db": -40}]));
        assert!(e.contains("unknown field") && e.contains("treshold_db"), "{e}");
        let e = bad(json!([{"type": "comp", "ratio": 0.5}]));
        assert!(e.contains("ratio"), "{e}");
        let e = bad(json!([{"type": "ir", "path": "c.wav", "mix": 2}]));
        assert!(e.contains("mix"), "{e}");
        let e = Rig::parse(&json!({"source": {"sfz": "a.sfz", "keyswitches": [[30, 20]]}}), "/", "x").unwrap_err().to_string();
        assert!(e.contains("keyswitches[0]"), "{e}");
        let e = Rig::parse(&json!({"source": {"sfz": "a.sfz", "init_cc": {"x": 1}}}), "/", "x").unwrap_err().to_string();
        assert!(e.contains("init_cc"), "{e}");
        let e = Rig::parse(&json!({"source": {"sfz": "a.sfz"}, "chian": []}), "/", "x").unwrap_err().to_string();
        assert!(e.contains("chian"), "{e}");
    }

    #[test]
    fn keyswitch_ranges_and_tuning() {
        let r = Rig::parse(
            &json!({"source": {"sfz": "a.sfz", "keyswitches": [[0, 29], [89, 127]], "tune_cents": -5}}),
            "/",
            "x",
        )
        .unwrap();
        assert!(r.source.is_keyswitch(17) && r.source.is_keyswitch(29) && r.source.is_keyswitch(127));
        assert!(!r.source.is_keyswitch(30) && !r.source.is_keyswitch(88));
        let f = r.source.tuning_frequency().unwrap();
        assert!((f - 440.0 * 2f64.powf(-5.0 / 1200.0)).abs() < 1e-12);
    }
}
