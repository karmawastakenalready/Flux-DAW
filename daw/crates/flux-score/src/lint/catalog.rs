//! Instrument cards (`catalog/*.json`, written by `tools/instrument_card.py`): which keys an instrument
//! plays, its keyswitches and whether it sounds at the written pitch.
//!
//! A card is matched to a track by the track's SFZ path (or one of the card's aliases), compared after
//! [`normalize_path`]; a path also matches when one ends with the other at a folder boundary, so an
//! absolute path finds the card of the same file.

use crate::theory::note::note_name;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// What the linter knows about an instrument.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InstrumentInfo {
    /// The card's name ("far_roads/flute").
    pub name: String,
    /// Where the information came from (the card's file name), for messages.
    pub source: String,
    /// The instrument's SFZ path and aliases, normalised.
    pub paths: Vec<String>,
    /// Runs of keys that sound, lowest first (keys between runs are silent).
    pub ranges: Vec<(u8, u8)>,
    /// Keyswitch keys and their labels.
    pub keyswitches: BTreeMap<u8, String>,
    /// Whether the instrument is pitched (None when the card does not say).
    pub pitched: Option<bool>,
    /// Sounding pitch minus played key in semitones, when the card measured one.
    pub offset: Option<i32>,
}

impl InstrumentInfo {
    /// An instrument that plays every key in `lo..=hi` (for ranges the caller knows without a card).
    pub fn with_range(name: impl Into<String>, lo: u8, hi: u8) -> InstrumentInfo {
        let name = name.into();
        InstrumentInfo {
            source: name.clone(),
            name,
            paths: vec![],
            ranges: vec![(lo.min(hi), lo.max(hi))],
            keyswitches: BTreeMap::new(),
            pitched: None,
            offset: None,
        }
    }

    /// Reads a card. `source` names it in messages (normally the file name).
    pub fn from_card(card: &Value, source: &str) -> Result<InstrumentInfo, String> {
        let name = card.get("name").and_then(Value::as_str).unwrap_or(source).to_string();
        let mut paths: Vec<String> = ["sfz", "sfz_path", "path", "file"]
            .iter()
            .find_map(|k| card.get(*k).and_then(Value::as_str))
            .or_else(|| card.get("instrument").and_then(|i| i.get("path")).and_then(Value::as_str))
            .map(normalize_path)
            .into_iter()
            .collect();
        if let Some(aliases) = card.get("aliases").and_then(Value::as_array) {
            paths.extend(aliases.iter().filter_map(Value::as_str).map(normalize_path));
        }
        let keys = card.get("keys").ok_or("no \"keys\" section")?;
        let pair = |v: &Value| -> Option<(u8, u8)> {
            let a = v.as_array()?;
            let (lo, hi) = (a.first()?.as_u64()?, a.get(1)?.as_u64()?);
            (a.len() == 2 && lo <= hi && hi <= 127).then_some((lo as u8, hi as u8))
        };
        let mut ranges: Vec<(u8, u8)> = match keys.get("ranges").and_then(Value::as_array) {
            Some(rs) => rs.iter().map(|r| pair(r).ok_or_else(|| format!("bad key range {r} in keys.ranges"))).collect::<Result<_, _>>()?,
            None => vec![],
        };
        if ranges.is_empty() {
            ranges.extend(keys.get("main").and_then(pair));
        }
        if ranges.is_empty() {
            return Err("no playable keys (keys.ranges or keys.main)".into());
        }
        ranges.sort_unstable();
        let mut keyswitches = BTreeMap::new();
        if let Some(ks) = card.get("keyswitches").and_then(|k| k.get("keys")).and_then(Value::as_object) {
            for (k, label) in ks {
                if let Some(key) = k.parse::<u8>().ok().filter(|&k| k <= 127) {
                    keyswitches.insert(key, label.as_str().unwrap_or_default().to_string());
                }
            }
        }
        let pitch = card.get("pitch");
        let pitched = pitch.and_then(|p| p.get("kind")).and_then(Value::as_str).and_then(|k| match k {
            "pitched" | "mixed" => Some(true),
            "unpitched" | "kit" => Some(false),
            _ => None,
        });
        let offset = pitch.and_then(|p| p.get("offset")).and_then(Value::as_i64).and_then(|o| i32::try_from(o).ok());
        Ok(InstrumentInfo { name, source: source.to_string(), paths, ranges, keyswitches, pitched, offset })
    }

    /// Whether `key` sounds (keyswitch keys do not count as playable).
    pub fn plays(&self, key: u8) -> bool {
        self.ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&key))
    }

    /// The lowest and highest playable keys.
    pub fn span(&self) -> (u8, u8) {
        let lo = self.ranges.iter().map(|r| r.0).min().unwrap_or(0);
        let hi = self.ranges.iter().map(|r| r.1).max().unwrap_or(127);
        (lo, hi)
    }

    /// "C4-C7", or the runs ("C2-B2, D3-F3") when there are gaps.
    pub fn describe_range(&self) -> String {
        self.ranges.iter().map(|&(lo, hi)| if lo == hi { note_name(lo) } else { format!("{}-{}", note_name(lo), note_name(hi)) }).collect::<Vec<_>>().join(", ")
    }
}

/// Instrument cards, looked up by SFZ path.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct InstrumentCatalog {
    cards: Vec<InstrumentInfo>,
}

impl InstrumentCatalog {
    pub fn new(cards: Vec<InstrumentInfo>) -> InstrumentCatalog {
        InstrumentCatalog { cards }
    }

    /// Loads every `*.json` card in `dir` (not recursive), in file-name order. Cards that cannot be read
    /// are skipped and listed with the reason; only an unreadable folder is an error.
    pub fn load_dir(dir: &Path) -> std::io::Result<(InstrumentCatalog, Vec<String>)> {
        let mut files: Vec<std::path::PathBuf> =
            std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json"))).collect();
        files.sort();
        let mut cards = Vec::new();
        let mut skipped = Vec::new();
        for f in files {
            let source = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let parsed = std::fs::read_to_string(&f).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()));
            match parsed.and_then(|v| InstrumentInfo::from_card(&v, &source)) {
                Ok(card) if card.paths.is_empty() => skipped.push(format!("{source}: no SFZ path")),
                Ok(card) => cards.push(card),
                Err(e) => skipped.push(format!("{source}: {e}")),
            }
        }
        Ok((InstrumentCatalog { cards }, skipped))
    }

    pub fn cards(&self) -> &[InstrumentInfo] {
        &self.cards
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// The card for an instrument path: an exact (normalised) match first, else the longest match where
    /// one path ends with the other at a folder boundary.
    pub fn find(&self, path: &str) -> Option<&InstrumentInfo> {
        let key = normalize_path(path);
        if key.is_empty() {
            return None;
        }
        if let Some(c) = self.cards.iter().find(|c| c.paths.contains(&key)) {
            return Some(c);
        }
        let tail_match = |a: &str, b: &str| -> Option<usize> {
            let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
            (long.ends_with(short) && (long.len() == short.len() || long.as_bytes()[long.len() - short.len() - 1] == b'/')).then_some(short.len())
        };
        self.cards.iter().filter_map(|c| c.paths.iter().filter_map(|p| tail_match(p, &key)).max().map(|n| (n, c))).max_by_key(|(n, _)| *n).map(|(_, c)| c)
    }
}

/// A path for comparison: `\` as `/`, lower case, no `.` parts, `..` folded, no leading `./`.
pub fn normalize_path(p: &str) -> String {
    let p = p.trim().replace('\\', "/").to_lowercase();
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|l| *l != "..") => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if absolute { format!("/{joined}") } else { joined }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card() -> Value {
        json!({
            "name": "x/cello", "sfz": "build/sfz/x/Cello.sfz", "aliases": ["build/sfz/y/cello.sfz"],
            "keys": {"main": [36, 77], "ranges": [[84, 90], [36, 77]]},
            "keyswitches": {"keys": {"120": "arco", "121": "pizz"}, "default": 120},
            "pitch": {"kind": "pitched", "offset": 12}
        })
    }

    #[test]
    fn reads_cards() {
        let c = InstrumentInfo::from_card(&card(), "x-cello.json").unwrap();
        assert_eq!(c.paths, ["build/sfz/x/cello.sfz", "build/sfz/y/cello.sfz"]);
        assert_eq!(c.ranges, [(36, 77), (84, 90)]);
        assert_eq!(c.span(), (36, 90));
        assert!(c.plays(36) && c.plays(85) && !c.plays(80) && !c.plays(120));
        assert_eq!(c.keyswitches.get(&121).map(String::as_str), Some("pizz"));
        assert_eq!((c.pitched, c.offset), (Some(true), Some(12)));
        assert_eq!(c.describe_range(), "C2-F5, C6-F#6");
        // the main range alone is enough
        let c = InstrumentInfo::from_card(&json!({"sfz": "a.sfz", "keys": {"main": [40, 50]}}), "a.json").unwrap();
        assert_eq!(c.ranges, [(40, 50)]);
        assert!(InstrumentInfo::from_card(&json!({"sfz": "a.sfz"}), "a.json").unwrap_err().contains("keys"));
        assert!(InstrumentInfo::from_card(&json!({"sfz": "a.sfz", "keys": {"ranges": [[50, 40]]}}), "a.json").is_err());
    }

    #[test]
    fn finds_cards_by_path() {
        let cat = InstrumentCatalog::new(vec![InstrumentInfo::from_card(&card(), "c.json").unwrap(), InstrumentInfo { paths: vec!["sfz/cello.sfz".into()], ..InstrumentInfo::with_range("other", 0, 1) }]);
        assert_eq!(cat.find("build\\sfz\\x\\CELLO.sfz").unwrap().name, "x/cello");
        assert_eq!(cat.find("build/sfz/y/cello.sfz").unwrap().name, "x/cello");
        assert_eq!(cat.find("C:/repo/build/sfz/x/cello.sfz").unwrap().name, "x/cello");
        assert_eq!(cat.find("./build/sfz/../build/sfz/x/cello.sfz").unwrap().name, "x/cello");
        // "sfz/cello.sfz" is a tail of neither x nor y's paths at a folder boundary... except its own card
        assert_eq!(cat.find("/abs/sfz/cello.sfz").unwrap().name, "other");
        assert!(cat.find("ello.sfz").is_none());
        assert!(cat.find("").is_none());
    }

    #[test]
    fn normalises_paths() {
        assert_eq!(normalize_path(" .\\A\\b\\..\\C.sfz "), "a/c.sfz");
        assert_eq!(normalize_path("/x//y/./z"), "/x/y/z");
        assert_eq!(normalize_path("../a"), "../a");
    }
}
