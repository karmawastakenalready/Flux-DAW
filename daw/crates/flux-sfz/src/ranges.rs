//! What an instrument maps: keys, keyswitches, velocity layers, round robins, release samples and controllers.
//!
//! [`ranges`] reads the [`Instrument`] model (so includes, inheritance and variables are taken into account,
//! unlike the old regex-based `tools/sfz_ranges.py`) and only counts the regions sfizz keeps (those whose
//! sample exists).

use crate::instrument::{Instrument, Opcode, Region};
use crate::opcode::{NamePattern, cc_name};
use crate::syntax::ItemKind;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// A range of keys or values, both ends included. Serialized as `[lo, hi]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Span(pub u8, pub u8);

/// The mapping of an instrument. See the [module documentation](self).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Ranges {
    pub regions: usize,
    /// Regions sfizz keeps (the sample exists, or a generator).
    pub playable_regions: usize,
    /// Keys that start a sound on note-on, as contiguous spans (gaps show as separate spans).
    pub keys: Vec<Span>,
    /// The distinct `pitch_keycenter`s of the note-on regions (a centre taken from the sample file is left out).
    pub keycenters: Vec<u8>,
    /// `sw_lokey`..`sw_hikey`: keys that switch articulations instead of playing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyswitch_range: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyswitch_default: Option<u8>,
    /// Articulations chosen with `sw_last` keyswitches, by key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keyswitches: Vec<Keyswitch>,
    /// Keys that must be held for some regions to play (`sw_down`), and released (`sw_up`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub held_keyswitches: Vec<u8>,
    /// The most velocity layers at one key (in one articulation): 1 means none. Ranges that span other ranges
    /// at the same key (a sample layered over every dynamic) are not counted.
    pub velocity_layers: usize,
    /// Every distinct velocity range (as heard: velocity crossfades narrow it) of the note-on regions.
    pub velocity_splits: Vec<Span>,
    /// Regions crossfade by velocity (`xfin_lovel`/`xfout_lovel`...).
    pub velocity_crossfades: bool,
    pub round_robin: RoundRobin,
    /// Regions played on note-off (`trigger=release` / `release_key`).
    pub release_regions: usize,
    /// Regions started by a controller (`start_locc`/`on_locc`) rather than a note.
    pub cc_triggered_regions: usize,
    /// Controllers the instrument responds to or names, by number.
    pub ccs: Vec<CcUse>,
}

/// One articulation selected by a keyswitch.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Keyswitch {
    /// The keyswitch (`sw_last`); `hi` is set when a range of keys selects it (`sw_lolast`..`sw_hilast`).
    pub key: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hi: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_from: Option<LabelSource>,
    /// Keys it plays.
    pub keys: Vec<Span>,
    pub velocity_layers: usize,
    pub regions: usize,
}

/// Where a keyswitch name was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    SwLabel,
    /// `label_keyN` in `<control>`.
    LabelKey,
    /// A comment next to the `sw_last` opcode or above its header.
    Comment,
}

/// Round robins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RoundRobin {
    /// Longest `seq_length` (1: none).
    pub sequence: u32,
    /// Most samples chosen at random (`lorand`/`hirand`) for one key and velocity (1: none).
    pub random: usize,
}

/// How an instrument uses one controller.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CcUse {
    pub cc: u32,
    /// The instrument's own name for it (`label_ccN`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The standard MIDI meaning, or sfizz's extended source (128 and up).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<&'static str>,
    /// Initial value (`set_ccN`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
    /// What it does: `condition` (regions play only inside a value range: articulation or mode switches),
    /// `crossfade` (`xfin`/`xfout`), `trigger`, or the parameter it modulates (`volume`, `cutoff`...).
    pub uses: Vec<String>,
}

/// A keyswitch (`sw_lolast`, `sw_hilast`), if the region needs one.
type Switch = Option<(u8, u8)>;
/// The velocity ranges that play each key, per keyswitch.
type LayerMap = BTreeMap<(Switch, u8), BTreeSet<Span>>;

/// The mapping of `inst`. See the [module documentation](self).
pub fn ranges(inst: &Instrument) -> Ranges {
    let playable: Vec<&Region> = inst.playable_regions().collect();
    let note_on: Vec<&Region> = playable.iter().copied().filter(|r| r.props().is_note_on()).collect();

    let mut keys = BTreeSet::new();
    let mut centers = BTreeSet::new();
    let mut splits = BTreeSet::new();
    // (switch, key) -> velocity ranges; (switch, key, velocity range) -> random ranges
    let mut layers: LayerMap = BTreeMap::new();
    let mut randoms: BTreeMap<(Switch, u8, Span), BTreeSet<(u64, u64)>> = BTreeMap::new();
    let mut rr = RoundRobin { sequence: 1, random: 1 };
    let mut xfade = false;
    for r in &note_on {
        let p = r.props();
        if p.lokey > p.hikey {
            continue;
        }
        if let Some(k) = p.keycenter() {
            centers.insert(k);
        }
        let (lo, hi) = p.audible_velocities();
        let vel = Span(lo, hi);
        splits.insert(vel);
        xfade |= p.xfin_hivel > p.xfin_lovel || p.xfout_hivel > p.xfout_lovel;
        rr.sequence = rr.sequence.max(p.seq_length);
        let rand = ((p.lorand * 1e6).round() as u64, (p.hirand * 1e6).round() as u64);
        for k in p.lokey..=p.hikey {
            keys.insert(k);
            layers.entry((p.sw_last, k)).or_default().insert(vel);
            if p.lorand > 0.0 || p.hirand < 1.0 {
                let set = randoms.entry((p.sw_last, k, vel)).or_default();
                set.insert(rand);
                rr.random = rr.random.max(set.len());
            }
        }
    }
    let velocity_layers = layers.values().map(layer_count).max().unwrap_or(0);

    let first = |f: &dyn Fn(&Region) -> Option<u8>| playable.iter().find_map(|r| f(r));
    let keyswitch_range = match (first(&|r| r.props().sw_lokey), first(&|r| r.props().sw_hikey)) {
        (Some(lo), Some(hi)) => Some(Span(lo, hi)),
        (Some(lo), None) => Some(Span(lo, 127)),
        (None, Some(hi)) => Some(Span(0, hi)),
        (None, None) => None,
    };
    let held: BTreeSet<u8> =
        playable.iter().flat_map(|r| [r.props().sw_down, r.props().sw_up]).flatten().collect();

    Ranges {
        regions: inst.regions().len(),
        playable_regions: playable.len(),
        keys: spans(&keys),
        keycenters: centers.into_iter().collect(),
        keyswitch_range,
        keyswitch_default: first(&|r| r.props().sw_default),
        keyswitches: keyswitches(inst, &note_on, &layers),
        held_keyswitches: held.into_iter().collect(),
        velocity_layers,
        velocity_splits: splits.into_iter().collect(),
        velocity_crossfades: xfade,
        round_robin: rr,
        release_regions: playable.iter().filter(|r| r.props().is_release()).count(),
        cc_triggered_regions: playable.iter().filter(|r| !r.props().cc_triggers.is_empty()).count(),
        ccs: cc_uses(inst, &playable),
    }
}

/// The velocity layers among the ranges that play one key: a range that spans another (a sample layered over
/// every dynamic, such as a noise or a body resonance) is not a layer of its own.
fn layer_count(ranges: &BTreeSet<Span>) -> usize {
    ranges.iter().filter(|a| !ranges.iter().any(|b| b != *a && a.0 <= b.0 && b.1 <= a.1)).count()
}

/// Contiguous runs of keys.
pub fn spans(keys: &BTreeSet<u8>) -> Vec<Span> {
    let mut out: Vec<Span> = vec![];
    for &k in keys {
        match out.last_mut() {
            Some(s) if u16::from(s.1) + 1 == u16::from(k) => s.1 = k,
            _ => out.push(Span(k, k)),
        }
    }
    out
}

fn keyswitches(
    inst: &Instrument,
    note_on: &[&Region],
    layers: &LayerMap,
) -> Vec<Keyswitch> {
    let listed = listed_keyswitch_names(inst);
    let mut by_switch: BTreeMap<(u8, u8), Vec<&Region>> = BTreeMap::new();
    for r in note_on {
        if let Some(sw) = r.props().sw_last {
            by_switch.entry(sw).or_default().push(r);
        }
    }
    by_switch
        .into_iter()
        .map(|((lo, hi), regions)| {
            let mut keys = BTreeSet::new();
            for r in &regions {
                let p = r.props();
                if p.lokey <= p.hikey {
                    keys.extend(p.lokey..=p.hikey);
                }
            }
            let velocity_layers =
                layers.iter().filter(|((sw, _), _)| *sw == Some((lo, hi))).map(|(_, v)| layer_count(v)).max().unwrap_or(0);
            let (label, label_from) = match keyswitch_label(inst, &regions, lo) {
                (None, _) => match listed.get(&lo) {
                    Some(l) => (Some(l.clone()), Some(LabelSource::Comment)),
                    None => (None, None),
                },
                found => found,
            };
            Keyswitch {
                key: lo,
                hi: (hi != lo).then_some(hi),
                label,
                label_from,
                keys: spans(&keys),
                velocity_layers,
                regions: regions.len(),
            }
        })
        .collect()
}

/// The name of an articulation: `sw_label`, else `label_keyN`, else a comment at its `sw_last`.
fn keyswitch_label(inst: &Instrument, regions: &[&Region], key: u8) -> (Option<String>, Option<LabelSource>) {
    if let Some(l) = regions.iter().find_map(|r| r.props().sw_label.clone().filter(|l| !l.trim().is_empty())) {
        return (Some(l), Some(LabelSource::SwLabel));
    }
    if let Some(l) = inst.control().label_key.get(&key) {
        return (Some(l.clone()), Some(LabelSource::LabelKey));
    }
    for r in regions {
        if let Some(op) = r.get("sw_last").or_else(|| r.get("sw_lolast"))
            && let Some(c) = comment_near(inst, op)
        {
            return (Some(c), Some(LabelSource::Comment));
        }
    }
    (None, None)
}

/// A comment on the same line as `op`, or on the line(s) right above the header of its block.
fn comment_near(inst: &Instrument, op: &Opcode) -> Option<String> {
    let file = inst.file(&op.location.file)?;
    let items = file.document.items();
    let at = items.iter().position(|i| i.line() == op.location.line && i.column() == op.location.column)?;
    // a comment later on the same line (`sw_last=36 // sustain`)
    for item in &items[at + 1..] {
        if item.lead().contains('\n') {
            break;
        }
        if let ItemKind::Comment(c) = item.kind() {
            return clean_comment(c);
        }
    }
    // the header of the block, then the comment line above it
    let header = (0..=at).rev().find(|&i| matches!(items[i].kind(), ItemKind::Header(_)))?;
    let prev = header.checked_sub(1)?;
    match items[prev].kind() {
        ItemKind::Comment(c) if items[header].lead().matches('\n').count() <= 1 => clean_comment(c),
        _ => None,
    }
}

/// Names given by a list in a comment, such as `// keyswitches: 120=sus, 121=trem` or
/// `// Keyswitches: C0: sustain; C#0: staccato` (keys as numbers or note names).
fn listed_keyswitch_names(inst: &Instrument) -> BTreeMap<u8, String> {
    let mut names = BTreeMap::new();
    for file in inst.files() {
        for item in file.document.items() {
            let ItemKind::Comment(c) = item.kind() else { continue };
            let lower = c.to_ascii_lowercase();
            let Some(at) = lower.find("keyswitch").or_else(|| lower.find("key switch")) else { continue };
            // after the word ("keyswitches:", "keyswitch -")
            let rest = c[at..].split_once([':', '-']).map_or("", |(_, r)| r);
            for part in rest.split([',', ';']) {
                let Some((key, label)) = part.split_once(['=', ':']) else { continue };
                let label = label.trim().trim_end_matches("*/").trim();
                if let Some(key) = crate::value::parse_key(key.trim()).filter(|_| !key.trim().contains(' '))
                    && !label.is_empty()
                    && label.len() <= 40
                    && label.chars().any(char::is_alphanumeric)
                {
                    names.entry(key).or_insert_with(|| label.to_owned());
                }
            }
        }
    }
    names
}

/// Comment text without delimiters; `None` for empty or decorative comments (`//-----`).
fn clean_comment(c: &str) -> Option<String> {
    let t = c.trim();
    let t = t.strip_prefix("//").map(str::trim).unwrap_or_else(|| t.trim_start_matches("/*").trim_end_matches("*/").trim());
    let t = t.trim_matches(|ch: char| matches!(ch, '-' | '=' | '*' | '#' | '/')).trim();
    (!t.is_empty() && t.chars().any(char::is_alphanumeric) && t.len() <= 60).then(|| t.to_owned())
}

/// What a controller opcode does: `condition`, `crossfade`, `trigger` or the modulated parameter.
fn cc_role(canonical: &str, cc: u32) -> String {
    for (prefix, role) in [
        ("locc", "condition"),
        ("hicc", "condition"),
        ("start_locc", "trigger"),
        ("start_hicc", "trigger"),
        ("stop_locc", "trigger"),
        ("stop_hicc", "trigger"),
        ("xfin_locc", "crossfade"),
        ("xfin_hicc", "crossfade"),
        ("xfout_locc", "crossfade"),
        ("xfout_hicc", "crossfade"),
    ] {
        if canonical.strip_prefix(prefix).is_some_and(|rest| rest == cc.to_string()) {
            return role.to_owned();
        }
    }
    // `volume_oncc1`, `cutoff_cc1`, `fileg_depthcc125`, `tune_curvecc1`: the part before the controller
    let suffix = format!("cc{cc}");
    let base = canonical.rfind(&suffix).map_or(canonical, |i| &canonical[..i]);
    let mut base = base.trim_end_matches('_');
    for modifier in ["_on", "_curve", "_smooth", "_step"] {
        if let Some(b) = base.strip_suffix(modifier) {
            base = b;
            break;
        }
    }
    let base = base.trim_end_matches('_');
    if base.is_empty() { "other".to_owned() } else { base.to_owned() }
}

fn cc_uses(inst: &Instrument, playable: &[&Region]) -> Vec<CcUse> {
    let mut uses: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    let mut seen_names = BTreeSet::new();
    for r in playable {
        for (_, o) in r.opcodes() {
            if !seen_names.insert(o.canonical.as_str()) {
                continue;
            }
            for cc in NamePattern::parse(&o.canonical).cc_numbers() {
                uses.entry(cc).or_default().insert(cc_role(&o.canonical, cc));
            }
        }
    }
    let control = inst.control();
    let mut numbers: BTreeSet<u32> = uses.keys().copied().collect();
    numbers.extend(control.label_cc.keys());
    numbers
        .into_iter()
        .map(|cc| {
            let u = uses.remove(&cc).unwrap_or_default();
            // conditions and crossfades first: they decide what plays
            let mut list: Vec<String> = u.iter().filter(|s| matches!(s.as_str(), "condition" | "crossfade" | "trigger")).cloned().collect();
            list.extend(u.into_iter().filter(|s| !matches!(s.as_str(), "condition" | "crossfade" | "trigger")));
            CcUse {
                cc,
                label: control.label_cc.get(&cc).cloned(),
                standard: cc_name(cc),
                default: control.set_cc.get(&cc).copied(),
                uses: list,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::LoadOptions;

    fn load(text: &str) -> Instrument {
        let opts = LoadOptions { check_samples: false, ..LoadOptions::default() };
        Instrument::from_text(text, std::env::temp_dir().join("flux-sfz-ranges.sfz"), &opts).unwrap()
    }

    #[test]
    fn keys_layers_and_round_robins() {
        let inst = load(
            "<control> label_cc1=Dynamics set_cc1=64 label_key24=Sustain (key label)\n\
             <global> sw_lokey=24 sw_hikey=26 sw_default=24 volume_oncc11=6\n\
             <group> sw_last=24 sw_label=Sus\n\
             <region> sample=a.wav lokey=48 hikey=52 pitch_keycenter=50 hivel=63 seq_length=2 seq_position=1\n\
             <region> sample=b.wav lokey=48 hikey=52 pitch_keycenter=50 hivel=63 seq_length=2 seq_position=2\n\
             <region> sample=c.wav lokey=48 hikey=52 pitch_keycenter=50 lovel=64\n\
             <region> sample=d.wav lokey=60 hikey=60 xfin_locc1=0 xfin_hicc1=127\n\
             <group> sw_last=25 // Staccato\n\
             <region> sample=e.wav key=55 lorand=0 hirand=0.5\n\
             <region> sample=f.wav key=55 lorand=0.5 hirand=1\n\
             <group> sw_last=26 trigger=release\n\
             <region> sample=g.wav key=55\n\
             <group> locc64=64\n<region> sample=h.wav key=70 cutoff_oncc74=1200\n\
             <group> sw_last=27\n<region> sample=i.wav key=50\n\
             <group> sw_last=28\n<region> sample=j.wav key=50\n\
             // keyswitches: 24=ignored, 27=Pizz, E1:Trem\n",
        );
        let r = ranges(&inst);
        assert_eq!(r.regions, 10);
        assert_eq!(r.keys, [Span(48, 52), Span(55, 55), Span(60, 60), Span(70, 70)]);
        assert_eq!(r.keycenters, [50, 55, 60, 70]);
        assert_eq!(r.keyswitch_range, Some(Span(24, 26)));
        assert_eq!(r.keyswitch_default, Some(24));
        assert_eq!(r.keyswitches.len(), 4);
        let names: Vec<_> = r.keyswitches.iter().map(|k| (k.key, k.label.as_deref())).collect();
        assert_eq!(names, [(24, Some("Sus")), (25, Some("Staccato")), (27, Some("Pizz")), (28, Some("Trem"))]);
        let sus = &r.keyswitches[0];
        assert_eq!((sus.key, sus.label.as_deref(), sus.label_from), (24, Some("Sus"), Some(LabelSource::SwLabel)));
        assert_eq!(sus.keys, [Span(48, 52), Span(60, 60)]);
        assert_eq!(sus.velocity_layers, 2);
        let stac = &r.keyswitches[1];
        assert_eq!((stac.label.as_deref(), stac.label_from), (Some("Staccato"), Some(LabelSource::Comment)));
        assert_eq!(r.velocity_layers, 2);
        assert_eq!(layer_count(&[Span(0, 62), Span(63, 127), Span(0, 127)].into()), 2);
        assert_eq!(layer_count(&[Span(0, 99), Span(71, 127)].into()), 2);
        assert_eq!(r.velocity_splits, [Span(0, 63), Span(0, 127), Span(64, 127)]);
        assert_eq!(r.round_robin, RoundRobin { sequence: 2, random: 2 });
        assert_eq!(r.release_regions, 1);
        let cc = |n: u32| r.ccs.iter().find(|c| c.cc == n).unwrap();
        assert_eq!(cc(1).uses, ["crossfade"]);
        assert_eq!((cc(1).label.as_deref(), cc(1).default), (Some("Dynamics"), Some(64.0)));
        assert_eq!(cc(11).uses, ["volume"]);
        assert_eq!(cc(64).uses, ["condition"]);
        assert_eq!(cc(74).uses, ["cutoff"]);
        assert_eq!(cc(74).standard, Some("brightness"));
    }

    #[test]
    fn roles_of_controller_opcodes() {
        assert_eq!(cc_role("volume_oncc1", 1), "volume");
        assert_eq!(cc_role("tune_curvecc1", 1), "tune");
        assert_eq!(cc_role("fileg_depthcc125", 125), "fileg_depth");
        assert_eq!(cc_role("lfo01_freq_oncc112", 112), "lfo01_freq");
        assert_eq!(cc_role("hicc105", 105), "condition");
        assert_eq!(cc_role("xfout_locc1", 1), "crossfade");
        assert_eq!(cc_role("start_locc64", 64), "trigger");
        assert_eq!(clean_comment("//----- Legato -----").as_deref(), Some("Legato"));
        assert_eq!(clean_comment("/* */"), None);
    }
}
