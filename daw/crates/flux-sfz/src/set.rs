//! Instrument sets: the SFZ instruments one song uses, defined as data (`sfz/sets/<set>.toml`).
//!
//! A set lists instruments by name. Each instrument plays a sample library either directly or through a
//! generated *wrapper* SFZ that adds controller handling (see [`crate::wrap`] for what each kind of wrapper
//! does), plus the facts the composing tools need (playable range, test notes...).
//!
//! ```toml
//! # Paths are relative to the project root (the folder that holds instruments/), like paths in projects.
//! [set]
//! description = "Capital City"
//! # output = "build/sfz/capital_city"   # where wrappers go; this is the default
//! # sample_paths = "relative"           # how wrappers point at the samples: "relative" or "absolute"
//!
//! [instruments.harp]
//! library = "VSCO-2-CE"
//! source = "instruments/VSCO-2-CE/Harp.sfz"
//! wrap = "expr"                          # expr | strings | keyswitch | patch | none
//! extra_global = { ampeg_release = "3.5" }
//! range = [28, 101]                      # MIDI keys or note names ("E1")
//! test_note = 62
//! gm = [0, 46]
//!
//! [instruments.violins]
//! wrap = "keyswitch"                     # one articulation per keyswitch, 120 upwards
//! articulations = [
//!     { name = "sus", source = "instruments/VSCO-2-CE/ViolinEnsSusVib.sfz", crossfade = true },
//!     { name = "pizz", source = "instruments/VSCO-2-CE/ViolinEnsPizz.sfz" },
//! ]
//!
//! [instruments.synth_bass]
//! source = "instruments/karoryfer.caveman-cosmonaut/Programs/main.sfz"
//! wrap = "patch"                         # a fixed copy of the program
//! patch = [
//!     { op = "replace", opcode = "loop_mode", value = "continuous", with = "loop_continuous" },
//!     { op = "insert_after", opcode = "cutoff", value = "250", opcodes = { cutoff_oncc120 = "6000" } },
//!     { op = "remove", opcodes = ["var01_*", "var02_*"] },
//! ]
//!
//! [instruments.chains]
//! from = "temporal_shift/percussion"     # start from another instrument ("<set>/<name>" or "<name>")
//! test_notes = [67, 68, 69, 82, 101]     # and change some fields
//! ```
//!
//! Unknown fields are errors, so a typo cannot silently change an instrument.

use crate::edit::SamplePaths;
use crate::error::{Error, Result};
use crate::fsutil::lexical_normalize;
use crate::value::parse_key;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The first keyswitch of a [`Wrap::Keyswitch`] instrument unless the set says otherwise.
pub const DEFAULT_KEYSWITCH_BASE: u8 = 120;

/// What is generated for an instrument. See [`crate::wrap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wrap {
    /// No wrapper: the source is played as it is.
    None,
    /// CC1/CC11 defaults and extra `<global>` opcodes.
    Expr,
    /// `expr`, plus a CC1 crossfade between the softest and loudest recorded layers and a CC1 filter.
    Strings,
    /// Several sources, one per articulation, chosen with keyswitches.
    Keyswitch,
    /// A copy of the program (with its included files) with opcode fixes.
    Patch,
}

/// An ordered list of opcodes, written as a TOML table (`{ ampeg_release = "3.5" }`). Values may be strings or
/// numbers; strings are taken exactly as written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpcodeList(pub Vec<(String, String)>);

impl Serialize for OpcodeList {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for OpcodeList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OpcodeList;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a table of opcodes (name = \"value\")")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<OpcodeList, A::Error> {
                let mut out = vec![];
                while let Some((k, v)) = map.next_entry::<String, ScalarText>()? {
                    if !crate::syntax::is_identifier(&k) {
                        return Err(de::Error::custom(format!("`{k}` is not an opcode name")));
                    }
                    out.push((k, v.0));
                }
                Ok(OpcodeList(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// A string, integer or float, as text.
struct ScalarText(String);

impl<'de> Deserialize<'de> for ScalarText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ScalarText;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or a number")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<ScalarText, E> {
                if v.contains(['\n', '\r']) || v.contains("//") || v.contains("/*") || v.contains('<') {
                    return Err(E::custom(format!("`{v}` cannot be an SFZ value (line break, comment or `<`)")));
                }
                Ok(ScalarText(v.trim().to_owned()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<ScalarText, E> {
                Ok(ScalarText(v.to_string()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<ScalarText, E> {
                Ok(ScalarText(v.to_string()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<ScalarText, E> {
                Ok(ScalarText(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

/// A MIDI key given as a number or a note name (`"C4"` = 60, `"F#3"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Key(pub u8);

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Key;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a MIDI key (0..127) or a note name such as \"C4\"")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Key, E> {
                u8::try_from(v).ok().filter(|k| *k <= 127).map(Key).ok_or_else(|| E::custom(format!("{v} is not a MIDI key")))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Key, E> {
                self.visit_i64(i64::try_from(v).unwrap_or(-1))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Key, E> {
                parse_key(v).map(Key).ok_or_else(|| E::custom(format!("`{v}` is not a MIDI key or note name")))
            }
        }
        d.deserialize_any(V)
    }
}

/// One articulation of a [`Wrap::Keyswitch`] instrument.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Articulation {
    /// Shown as the keyswitch's `sw_label`.
    pub name: String,
    pub source: String,
    /// Crossfade the dynamic layers with CC1 (the `strings` treatment) instead of `expr`.
    #[serde(default)]
    pub crossfade: bool,
}

/// One fix of a [`Wrap::Patch`] instrument, applied to every opcode of the program and its included files.
/// Opcode names match case-insensitively (aliases count: `loopmode` matches `loop_mode`); values match exactly
/// (after trimming); `value` may be left out to match any value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatchOp {
    /// Change a value.
    Replace {
        opcode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        with: String,
    },
    /// Add opcodes after an opcode (one per line).
    InsertAfter {
        opcode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        opcodes: OpcodeList,
    },
    /// Remove opcodes (with comments on the same line). Names may use `*` as a wildcard.
    Remove { opcodes: Vec<String> },
}

/// An instrument of a set, after `from` was resolved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentSpec {
    /// The sample library (folder name under instruments/), for licensing and documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    /// The SFZ to play or wrap (relative to the project root). Not used by keyswitch instruments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub wrap: Wrap,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub articulations: Vec<Articulation>,
    /// First keyswitch key (default 120): articulation `i` is `keyswitch_base + i`; the first is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyswitch_base: Option<Key>,
    /// Opcodes added to the `<global>` header(s) of the wrapper.
    #[serde(default, skip_serializing_if = "is_empty_list")]
    pub extra_global: OpcodeList,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patch: Vec<PatchOp>,
    /// Lowest and highest key the composer may write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[Key; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_note: Option<Key>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_notes: Option<Vec<Key>>,
    /// Longest sensible note in seconds (breath, bow length).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_note_s: Option<f64>,
    /// General MIDI fallback: [bank, program].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gm: Option<[u16; 2]>,
    /// MIDI channel (0-based) in exported MIDI files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    /// Rendered reversed (each note's attack lands on the note's end).
    #[serde(default, skip_serializing_if = "is_false")]
    pub reverse: bool,
    /// Articulation keyswitches of the source itself (name -> key), for sources that have their own.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ks_map: BTreeMap<String, Key>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_empty_list(l: &OpcodeList) -> bool {
    l.0.is_empty()
}

/// An instrument set file, loaded and checked.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SetFile {
    /// The file name without `.toml`.
    pub name: String,
    pub path: PathBuf,
    /// The project root the paths in the file are relative to.
    pub root: PathBuf,
    pub description: Option<String>,
    /// Where the wrappers go (absolute).
    pub output: PathBuf,
    pub sample_paths: SamplePaths,
    /// In file order.
    pub instruments: Vec<(String, InstrumentSpec)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetHeader {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    sample_paths: Option<SamplePaths>,
}

impl SetFile {
    /// Load `path`; relative paths inside are relative to `root`. `from = "<set>/<name>"` references are looked
    /// up in the same folder as `path`.
    pub fn load(path: &Path, root: &Path) -> Result<SetFile> {
        let path = std::path::absolute(path).map(|p| lexical_normalize(&p)).map_err(|e| Error::io(path, e))?;
        let root = std::path::absolute(root).map(|p| lexical_normalize(&p)).map_err(|e| Error::io(root, e))?;
        let name = set_name(&path)?;
        let table = read_table(&path)?;
        let mut loader = FromResolver { dir: path.parent().map(Path::to_path_buf).unwrap_or_default(), tables: vec![(name.clone(), table.clone())], stack: vec![] };
        let header: SetHeader = match table.get("set") {
            Some(v) => v.clone().try_into().map_err(|e| Error::set(&path, format!("[set]: {}", toml_message(&e))))?,
            None => SetHeader { description: None, output: None, sample_paths: None },
        };
        for key in table.keys() {
            if key != "set" && key != "instruments" {
                return Err(Error::set(&path, format!("unknown section `{key}` (expected [set] and [instruments.<name>])")));
            }
        }
        let names: Vec<String> = match table.get("instruments") {
            Some(toml::Value::Table(t)) => t.keys().cloned().collect(),
            Some(_) => return Err(Error::set(&path, "`instruments` must be a table of [instruments.<name>] sections")),
            None => vec![],
        };
        let mut instruments = vec![];
        for n in names {
            if !crate::syntax::is_identifier(&n) {
                return Err(Error::set(&path, format!("instrument name `{n}`: use letters, digits and `_` (it names a file)")));
            }
            let merged = loader.resolve(&name, &n).map_err(|m| Error::set(&path, format!("instrument `{n}`: {m}")))?;
            let spec: InstrumentSpec = toml::Value::Table(merged)
                .try_into()
                .map_err(|e| Error::set(&path, format!("instrument `{n}`: {}", toml_message(&e))))?;
            validate(&spec).map_err(|m| Error::set(&path, format!("instrument `{n}`: {m}")))?;
            instruments.push((n, spec));
        }
        let output = match &header.output {
            Some(o) => lexical_normalize(&root.join(o)),
            None => root.join("build").join("sfz").join(&name),
        };
        Ok(SetFile {
            name,
            path,
            root,
            description: header.description,
            output,
            sample_paths: header.sample_paths.unwrap_or_default(),
            instruments,
        })
    }

    /// Load every `*.toml` set in a folder, sorted by name.
    pub fn load_dir(dir: &Path, root: &Path) -> Result<Vec<SetFile>> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| Error::io(dir, e))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        paths.iter().map(|p| SetFile::load(p, root)).collect()
    }

    /// The sets of a folder named in `names` (a name may end in `.toml`), in that order; every set when `names`
    /// is empty.
    pub fn load_named(dir: &Path, root: &Path, names: &[String]) -> Result<Vec<SetFile>> {
        if !dir.is_dir() {
            return Err(Error::Invalid(format!("no set definitions: {} is not a folder (sets are sfz/sets/<set>.toml)", dir.display())));
        }
        let all = SetFile::load_dir(dir, root)?;
        if names.is_empty() {
            return Ok(all);
        }
        names
            .iter()
            .map(|n| {
                let n = n.trim_end_matches(".toml");
                all.iter().find(|s| s.name == n).cloned().ok_or_else(|| {
                    let known: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
                    Error::Invalid(format!("no set `{n}` in {} (sets: {})", dir.display(), known.join(", ")))
                })
            })
            .collect()
    }

    pub fn instrument(&self, name: &str) -> Option<&InstrumentSpec> {
        self.instruments.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// A path from the set file, made absolute.
    pub fn resolve(&self, rel: &str) -> PathBuf {
        lexical_normalize(&self.root.join(rel.replace('\\', "/")))
    }
}

fn set_name(path: &Path) -> Result<String> {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if !crate::syntax::is_identifier(&stem) {
        return Err(Error::set(path, "the set file name must use letters, digits and `_` (it names the output folder)"));
    }
    Ok(stem)
}

fn read_table(path: &Path) -> Result<toml::Table> {
    let bytes = crate::error::read_file(path)?;
    let text = String::from_utf8(bytes).map_err(|_| Error::set(path, "not UTF-8 text"))?;
    text.parse::<toml::Table>().map_err(|e| Error::set(path, toml_message(&e)))
}

/// A TOML error as one line (line and column included when known).
fn toml_message(e: &dyn fmt::Display) -> String {
    e.to_string().lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('|')).collect::<Vec<_>>().join(" ")
}

/// Resolves `from` chains across set files.
struct FromResolver {
    dir: PathBuf,
    /// set name -> parsed file
    tables: Vec<(String, toml::Table)>,
    /// (set, instrument) being resolved, for cycles
    stack: Vec<(String, String)>,
}

impl FromResolver {
    fn table(&mut self, set: &str) -> std::result::Result<&toml::Table, String> {
        if !self.tables.iter().any(|(n, _)| n == set) {
            let p = self.dir.join(format!("{set}.toml"));
            let t = read_table(&p).map_err(|e| format!("from: set `{set}`: {e}"))?;
            self.tables.push((set.to_owned(), t));
        }
        Ok(&self.tables.iter().find(|(n, _)| n == set).expect("just added").1)
    }

    /// The instrument's table with `from` applied (base first, then this table's fields on top).
    fn resolve(&mut self, set: &str, name: &str) -> std::result::Result<toml::Table, String> {
        let key = (set.to_owned(), name.to_owned());
        if self.stack.contains(&key) {
            let chain: Vec<String> = self.stack.iter().chain([&key]).map(|(s, n)| format!("{s}/{n}")).collect();
            return Err(format!("`from` loops: {}", chain.join(" -> ")));
        }
        let table = self.table(set)?;
        let own = match table.get("instruments").and_then(|i| i.get(name)) {
            Some(toml::Value::Table(t)) => t.clone(),
            Some(_) => return Err(format!("{set}/{name} must be a table")),
            None => return Err(format!("from: there is no instrument `{name}` in set `{set}`")),
        };
        let Some(from) = own.get("from") else { return Ok(own) };
        let from = from.as_str().ok_or("`from` must be a string: \"<set>/<instrument>\" or \"<instrument>\"")?.to_owned();
        let (bset, bname) = match from.split_once('/') {
            Some((s, n)) => (s.to_owned(), n.to_owned()),
            None => (set.to_owned(), from.clone()),
        };
        self.stack.push(key);
        let base = self.resolve(&bset, &bname);
        self.stack.pop();
        let mut merged = base?;
        for (k, v) in own {
            if k != "from" {
                merged.insert(k, v);
            }
        }
        Ok(merged)
    }
}

fn validate(spec: &InstrumentSpec) -> std::result::Result<(), String> {
    match spec.wrap {
        Wrap::Keyswitch => {
            if spec.articulations.is_empty() {
                return Err("wrap = \"keyswitch\" needs `articulations`".into());
            }
            if spec.source.is_some() {
                return Err("a keyswitch instrument takes its sources from `articulations`, not `source`".into());
            }
            let base = spec.keyswitch_base.map_or(DEFAULT_KEYSWITCH_BASE, |k| k.0);
            if usize::from(base) + spec.articulations.len() > 128 {
                return Err(format!("{} articulations do not fit above keyswitch {base}", spec.articulations.len()));
            }
            let mut names: Vec<&str> = spec.articulations.iter().map(|a| a.name.as_str()).collect();
            names.sort_unstable();
            if names.windows(2).any(|w| w[0] == w[1]) {
                return Err("articulation names must be unique".into());
            }
        }
        _ => {
            if spec.source.is_none() {
                return Err("`source` is required".into());
            }
            if !spec.articulations.is_empty() {
                return Err("`articulations` only applies to wrap = \"keyswitch\"".into());
            }
        }
    }
    if !spec.patch.is_empty() && spec.wrap != Wrap::Patch {
        return Err("`patch` only applies to wrap = \"patch\"".into());
    }
    if !spec.extra_global.0.is_empty() && matches!(spec.wrap, Wrap::None | Wrap::Patch) {
        return Err("`extra_global` needs a generated wrapper (expr, strings or keyswitch)".into());
    }
    if let Some([lo, hi]) = spec.range
        && lo > hi
    {
        return Err(format!("range [{}, {}] is reversed", lo.0, hi.0));
    }
    if spec.max_note_s.is_some_and(|s| !(s.is_finite() && s > 0.0)) {
        return Err("max_note_s must be a positive number of seconds".into());
    }
    if spec.channel.is_some_and(|c| c > 15) {
        return Err("channel must be 0..15".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flux-sfz-set-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_and_resolves_from() {
        let d = dir("from");
        std::fs::write(
            d.join("base.toml"),
            "[instruments.perc]\nsource = \"a/b.sfz\"\nwrap = \"expr\"\nrange = [\"C2\", 104]\nextra_global = { ampeg_release = \"3\", x = 1.5 }\n",
        )
        .unwrap();
        std::fs::write(
            d.join("song.toml"),
            "[set]\noutput = \"out/x\"\nsample_paths = \"absolute\"\n\n[instruments.chains]\nfrom = \"base/perc\"\ntest_notes = [67, \"G#4\"]\n\n\
             [instruments.v]\nwrap = \"keyswitch\"\narticulations = [{ name = \"sus\", source = \"s.sfz\", crossfade = true }, { name = \"pizz\", source = \"p.sfz\" }]\n\n\
             [instruments.syn]\nsource = \"c.sfz\"\nwrap = \"patch\"\npatch = [{ op = \"replace\", opcode = \"loop_mode\", value = \"continuous\", with = \"loop_continuous\" }, { op = \"remove\", opcodes = [\"var0*\"] }]\n",
        )
        .unwrap();
        let s = SetFile::load(&d.join("song.toml"), &d).unwrap();
        assert_eq!(s.name, "song");
        assert_eq!(s.output, d.join("out").join("x"));
        assert_eq!(s.sample_paths, SamplePaths::Absolute);
        let names: Vec<&str> = s.instruments.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["chains", "v", "syn"]);
        let c = s.instrument("chains").unwrap();
        assert_eq!(c.source.as_deref(), Some("a/b.sfz"));
        assert_eq!(c.range, Some([Key(36), Key(104)]));
        assert_eq!(c.test_notes, Some(vec![Key(67), Key(68)]));
        assert_eq!(c.extra_global.0, vec![("ampeg_release".into(), "3".into()), ("x".into(), "1.5".into())]);
        assert_eq!(s.instrument("v").unwrap().articulations.len(), 2);
        assert_eq!(s.instrument("syn").unwrap().patch.len(), 2);
    }

    #[test]
    fn mistakes_are_reported() {
        let d = dir("bad");
        let bad = |text: &str| -> String {
            std::fs::write(d.join("s.toml"), text).unwrap();
            SetFile::load(&d.join("s.toml"), &d).unwrap_err().to_string()
        };
        assert!(bad("[instruments.a]\nsource = \"x\"\nwrap = \"expr\"\nrnage = [1, 2]\n").contains("unknown field `rnage`"));
        assert!(bad("[instruments.a]\nwrap = \"expr\"\n").contains("`source` is required"));
        assert!(bad("[instruments.a]\nsource = \"x\"\nwrap = \"fancy\"\n").contains("instrument `a`"));
        assert!(bad("[instruments.a]\nfrom = \"b\"\n[instruments.b]\nfrom = \"a\"\n").contains("`from` loops"));
        assert!(bad("[instruments.a]\nfrom = \"other/b\"\n").contains("set `other`"));
        assert!(bad("[instruments.a]\nsource = \"x\"\nwrap = \"expr\"\nrange = [80, 20]\n").contains("reversed"));
        assert!(bad("[instruments.a]\nsource = \"x\"\nwrap = \"expr\"\nextra_global = { a = \"1 // x\" }\n").contains("cannot be an SFZ value"));
        assert!(bad("[instruments.a]\nsource = \"x\"\nwrap = \"patch\"\npatch = [{ op = \"rename\" }]\n").contains("instrument `a`"));
        assert!(bad("[sets]\n").contains("unknown section"));
        assert!(bad("[instruments.a\n").contains("s.toml"));
    }
}
