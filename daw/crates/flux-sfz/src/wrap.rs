//! Wrapper generation: the SFZ files an instrument set's instruments are played through.
//!
//! The wrappers leave the libraries untouched: they are new files next to each other in the set's output
//! folder, pointing at the original samples through `default_path` (see [`crate::edit`]). What each
//! [`Wrap`] does:
//!
//! * `none`: nothing is generated; the source is played directly.
//! * `expr`: the source with, in every `<control>`, `set_cc1=64` and `set_cc11=127` (sfizz handles CC7/CC11
//!   itself), and the instrument's `extra_global` opcodes added to its `<global>`.
//! * `strings`: `expr` plus a CC1 dynamics crossfade. The recorded dynamic layer of each region is read from
//!   its sample name (`_v3` -> 3, `_p_`/`_ff.` -> 2/6 on the scale pp=1 .. ff=6). Only the softest and the
//!   loudest layers are kept, one region per layer and key range, without velocity splits; the soft regions
//!   fade out and the loud ones fade in while CC1 goes from 40 to 100 (power curve). The `<global>` gets a
//!   gentle CC1-controlled low-pass filter (`cutoff=4500`, `cutoff_oncc1=2000`), `volume_oncc1=6`,
//!   `amp_veltrack=45`, a 60 ms attack and a 0.5 s release.
//! * `keyswitch`: one instrument holding several sources, each made into an `expr` or `strings` (`crossfade`)
//!   wrapper whose `<global>` becomes a `<master>` that answers keyswitch `keyswitch_base + i` (120 by default;
//!   the first articulation plays until another keyswitch is pressed). Each master carries an `sw_label` with
//!   the articulation's name.
//! * `patch`: a copy of the program and the files it includes (same layout, so `#include` lines keep working)
//!   with the set's opcode fixes applied.
//!
//! These are the rules of `instruments.py` (which generated `build/sfz/` before), with these differences:
//! wrappers are read with the full SFZ parser (includes, variables, values with spaces), so they work for any
//! source; each `<control>` keeps its own `default_path` instead of all taking the file's first one; a
//! `<control>` or `<global>` is added when the source has none (instead of silently not applying the
//! settings); a source `<master>` keeps the keyswitch; `sw_label`s name the articulations; and the files use
//! `\n` line ends on every system (patched copies keep the line ends of their source).

use crate::edit::{Derived, Layout, OpcodeEdit, OutputFile, SamplePaths};
use crate::error::{Diagnostic, Error, Location, Result, Severity};
use crate::fsutil::{relative_path, slash_path};
use crate::instrument::{Instrument, LoadOptions};
use crate::opcode::{HeaderKind, canonical_name};
use crate::set::{DEFAULT_KEYSWITCH_BASE, InstrumentSpec, OpcodeList, PatchOp, SetFile, Wrap};
use crate::syntax::Document;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// CC1 window of the `strings` crossfade.
pub const XFADE_LO: u8 = 40;
pub const XFADE_HI: u8 = 100;

/// The `<global>` additions of the `strings` wrap.
const STRINGS_GLOBAL: [(&str, &str); 8] = [
    ("xf_cccurve", "power"),
    ("amp_veltrack", "45"),
    ("fil_type", "lpf_2p"),
    ("cutoff", "4500"),
    ("cutoff_oncc1", "2000"),
    ("volume_oncc1", "6"),
    ("ampeg_attack", "0.06"),
    ("ampeg_release", "0.5"),
];

/// What was generated for one instrument of a set.
#[derive(Clone, Debug)]
pub struct Generated {
    pub name: String,
    pub wrap: Wrap,
    /// The SFZ to load: the wrapper, or the source itself for `wrap = "none"`.
    pub load: PathBuf,
    /// The files to write (none for `wrap = "none"`).
    pub files: Vec<OutputFile>,
    /// Every file read to make them (the sources and what they include).
    pub inputs: Vec<PathBuf>,
    /// Errors in the sources (missing samples...) and warnings about the result.
    pub diagnostics: Vec<Diagnostic>,
}

/// Summary of a [`Generated`] for reports.
#[derive(Clone, Debug, Serialize)]
pub struct GeneratedSummary {
    pub name: String,
    pub wrap: Wrap,
    pub load: PathBuf,
    pub outputs: Vec<PathBuf>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Generated {
    pub fn summary(&self) -> GeneratedSummary {
        GeneratedSummary {
            name: self.name.clone(),
            wrap: self.wrap,
            load: self.load.clone(),
            outputs: self.files.iter().map(|f| f.path.clone()).collect(),
            diagnostics: self.diagnostics.clone(),
        }
    }
}

/// Generate one instrument of a set (nothing is written).
pub fn generate(set: &SetFile, name: &str) -> Result<Generated> {
    let spec = set.instrument(name).ok_or_else(|| Error::Invalid(format!("set `{}` has no instrument `{name}`", set.name)))?;
    let mut g = Gen { set, name, diags: vec![], inputs: BTreeSet::new() };
    let (load, files) = match spec.wrap {
        Wrap::None => (set.resolve(source(spec)?), vec![]),
        Wrap::Expr | Wrap::Strings => g.single(spec)?,
        Wrap::Keyswitch => g.keyswitch(spec)?,
        Wrap::Patch => g.patch(spec)?,
    };
    if spec.wrap == Wrap::None {
        // the source is played as it is: report its problems, and depend on it
        g.load(&load)?;
    }
    Ok(Generated { name: name.to_owned(), wrap: spec.wrap, load, files, inputs: g.inputs.into_iter().collect(), diagnostics: g.diags })
}

fn source(spec: &InstrumentSpec) -> Result<&str> {
    spec.source.as_deref().ok_or_else(|| Error::Invalid("the instrument has no source".into()))
}

/// One block of a wrapper being built.
#[derive(Clone, Debug)]
struct OutBlock {
    kind: HeaderKind,
    header: String,
    ops: Vec<(String, String)>,
}

impl OutBlock {
    fn new(kind: HeaderKind, header: &str, ops: Vec<(String, String)>) -> OutBlock {
        OutBlock { kind, header: header.to_owned(), ops }
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
}

struct Gen<'s> {
    set: &'s SetFile,
    name: &'s str,
    diags: Vec<Diagnostic>,
    inputs: BTreeSet<PathBuf>,
}

impl Gen<'_> {
    fn load(&mut self, path: &Path) -> Result<Instrument> {
        let inst = Instrument::load_with(path, &LoadOptions::default())?;
        self.inputs.extend(inst.files().iter().map(|f| f.path.clone()));
        self.diags.extend(inst.errors().cloned());
        Ok(inst)
    }

    fn warn(&mut self, file: &Path, message: String) {
        self.diags.push(Diagnostic { severity: Severity::Warning, location: Location::new(file, 1, 1), message });
    }

    fn rel(&self, path: &Path) -> String {
        relative_path(&self.set.root, path).unwrap_or_else(|| slash_path(path))
    }

    /// The `default_path` prefix that leads from `out_dir` to the folder of `inst`.
    fn prefix(&self, inst: &Instrument, out_dir: &Path) -> String {
        let src = inst.dir();
        let absolute = format!("{}/", slash_path(src).trim_end_matches('/'));
        match self.set.sample_paths {
            SamplePaths::Absolute => absolute,
            SamplePaths::Relative => match relative_path(out_dir, src) {
                Some(r) if r.is_empty() => String::new(),
                Some(r) => format!("{r}/"),
                None => absolute,
            },
        }
    }

    fn single(&mut self, spec: &InstrumentSpec) -> Result<(PathBuf, Vec<OutputFile>)> {
        let src = self.set.resolve(source(spec)?);
        let out = self.set.output.join(format!("{}.sfz", self.name));
        let inst = self.load(&src)?;
        let prefix = self.prefix(&inst, &self.set.output);
        let blocks = self.wrapper_blocks(&inst, spec.wrap == Wrap::Strings, &spec.extra_global, &prefix)?;
        let comment = format!(
            "// generated by flux-sfz from {} (set {}, wrap {}) - edit the set, not this file",
            self.rel(&src),
            self.set.name,
            wrap_name(spec.wrap)
        );
        let text = emit(&comment, true, &blocks);
        Ok((out.clone(), vec![output_file(out, src, &text)]))
    }

    fn keyswitch(&mut self, spec: &InstrumentSpec) -> Result<(PathBuf, Vec<OutputFile>)> {
        let base = spec.keyswitch_base.map_or(DEFAULT_KEYSWITCH_BASE, |k| k.0);
        let n = spec.articulations.len();
        let hikey = (usize::from(base) + 7.max(n - 1)).min(127);
        let out = self.set.output.join(format!("{}.sfz", self.name));
        let mut blocks = vec![];
        for (i, art) in spec.articulations.iter().enumerate() {
            let src = self.set.resolve(&art.source);
            let inst = self.load(&src)?;
            let prefix = self.prefix(&inst, &self.set.output);
            let sub = self.wrapper_blocks(&inst, art.crossfade, &spec.extra_global, &prefix)?;
            let sw = vec![
                ("sw_lokey".to_owned(), base.to_string()),
                ("sw_hikey".to_owned(), hikey.to_string()),
                ("sw_last".to_owned(), (usize::from(base) + i).to_string()),
                ("sw_default".to_owned(), base.to_string()),
                ("sw_label".to_owned(), art.name.clone()),
            ];
            let mut had_master = false;
            let mut global: Vec<(String, String)> = vec![];
            for b in sub {
                match b.kind {
                    HeaderKind::Global => {
                        blocks.push(OutBlock::new(HeaderKind::Master, "master", [sw.clone(), b.ops.clone()].concat()));
                        global = b.ops;
                        had_master = true;
                    }
                    // a <master> of the source replaces ours: give it the keyswitch and the global opcodes
                    HeaderKind::Master => {
                        blocks.push(OutBlock::new(HeaderKind::Master, "master", [sw.clone(), global.clone(), b.ops].concat()));
                        had_master = true;
                    }
                    HeaderKind::Group | HeaderKind::Region => {
                        if !had_master {
                            blocks.push(OutBlock::new(HeaderKind::Master, "master", sw.clone()));
                            had_master = true;
                        }
                        blocks.push(b);
                    }
                    _ => blocks.push(b),
                }
            }
        }
        let legend: Vec<String> =
            spec.articulations.iter().enumerate().map(|(i, a)| format!("{}={}", usize::from(base) + i, a.name)).collect();
        let comment = format!(
            "// generated by flux-sfz (set {}): multi-articulation instrument '{}' - keyswitches: {}",
            self.set.name,
            self.name,
            legend.join(", ")
        );
        let text = emit(&comment, false, &blocks);
        let first = self.set.resolve(&spec.articulations[0].source);
        Ok((out.clone(), vec![output_file(out, first, &text)]))
    }

    fn patch(&mut self, spec: &InstrumentSpec) -> Result<(PathBuf, Vec<OutputFile>)> {
        let src = self.set.resolve(source(spec)?);
        let file_name = src.file_name().ok_or_else(|| Error::Invalid(format!("{} is not a file", src.display())))?;
        let out = self.set.output.join(self.name).join(file_name);
        let inst = self.load(&src)?;
        let mut der = Derived::new(&inst, &out, Layout::Mirror, self.set.sample_paths)?;
        for (i, op) in spec.patch.iter().enumerate() {
            let n = der.edit_opcodes(|name, value| apply_patch_op(op, name, value));
            if n == 0 {
                self.warn(&src, format!("patch operation {} ({}) matched nothing", i + 1, describe_op(op)));
            }
        }
        der.set_header_comment(&[format!(
            "generated by flux-sfz from {} (set {}, wrap patch) - edit the set, not this file",
            self.rel(&src),
            self.set.name
        )]);
        let files = der.finish()?;
        Ok((out, files))
    }

    /// The source's blocks with the `expr` (and, if `crossfade`, `strings`) treatment.
    fn wrapper_blocks(
        &mut self,
        inst: &Instrument,
        crossfade: bool,
        extra_global: &OpcodeList,
        prefix: &str,
    ) -> Result<Vec<OutBlock>> {
        let mut out: Vec<OutBlock> = vec![];
        for b in inst.blocks() {
            let mut ops: Vec<(String, String)> = b.opcodes.iter().map(|o| (o.name.clone(), o.value.clone())).collect();
            match b.kind {
                HeaderKind::Control => {
                    // the block's own default_path (the last one wins), prefixed so it still finds the samples
                    let dp = b.opcodes.iter().rev().find(|o| o.canonical == "default_path").map(|o| o.value.replace('\\', "/"));
                    ops.retain(|(n, _)| canonical_name(n) != "default_path");
                    ops.insert(0, ("default_path".into(), relocated(prefix, dp.as_deref().unwrap_or(""))));
                    ops.extend(pairs(&[("set_cc1", "64"), ("set_cc11", "127")]));
                }
                HeaderKind::Global => {
                    if crossfade {
                        ops.extend(pairs(&STRINGS_GLOBAL));
                    }
                    ops.extend(extra_global.0.iter().cloned());
                }
                _ => {}
            }
            out.push(OutBlock::new(b.kind, &b.name, ops));
        }
        // a source without <control> (or with regions before it) still needs the default_path and CC defaults
        let first_control = out.iter().position(|b| b.kind == HeaderKind::Control);
        let first_region = out.iter().position(|b| b.kind == HeaderKind::Region);
        if first_region.is_some() && first_control.is_none_or(|c| Some(c) > first_region) {
            let ops = vec![
                ("default_path".to_owned(), relocated(prefix, "")),
                ("set_cc1".to_owned(), "64".to_owned()),
                ("set_cc11".to_owned(), "127".to_owned()),
            ];
            out.insert(0, OutBlock::new(HeaderKind::Control, "control", ops));
        }
        // and a source without <global> still gets the global additions
        let additions: Vec<(String, String)> =
            [if crossfade { pairs(&STRINGS_GLOBAL) } else { vec![] }, extra_global.0.clone()].concat();
        if !additions.is_empty() && !out.iter().any(|b| b.kind == HeaderKind::Global) {
            let at = out
                .iter()
                .position(|b| matches!(b.kind, HeaderKind::Master | HeaderKind::Group | HeaderKind::Region))
                .unwrap_or(out.len());
            out.insert(at, OutBlock::new(HeaderKind::Global, "global", additions));
        }
        if crossfade {
            // region blocks are in the same order as the instrument's regions
            let regions: Vec<(usize, usize)> = out
                .iter()
                .enumerate()
                .filter(|(_, b)| b.kind == HeaderKind::Region)
                .map(|(pos, _)| pos)
                .zip(0..)
                .collect();
            self.crossfade(inst, &mut out, &regions)?;
        }
        Ok(out)
    }

    /// Keep the softest and loudest recorded layers and crossfade them with CC1.
    fn crossfade(&mut self, inst: &Instrument, out: &mut Vec<OutBlock>, regions: &[(usize, usize)]) -> Result<()> {
        let layer_of = |r: usize| inst.regions()[r].value("sample").and_then(dynamic_layer);
        let layers: BTreeSet<u8> = regions.iter().filter_map(|&(_, r)| layer_of(r)).collect();
        let (Some(&soft), Some(&loud)) = (layers.first(), layers.last()) else {
            return Err(Error::Sfz {
                location: Location::new(inst.path(), 1, 1),
                message: "wrap = \"strings\" needs dynamic layers in the sample names (_v1.._v9 or _pp/_p/_mp/_mf/_f/_ff), \
                          and none were found"
                    .into(),
            });
        };
        let mut seen = BTreeSet::new();
        let mut drop = BTreeSet::new();
        let (mut soft_keys, mut loud_keys) = (BTreeSet::new(), BTreeSet::new());
        for &(pos, r) in regions {
            let layer = layer_of(r).unwrap_or(soft);
            let p = inst.regions()[r].props();
            if (layer != soft && layer != loud) || !seen.insert((layer, p.lokey, p.hikey)) {
                // a middle layer, or another take of the same note and layer (keeping both would double it)
                drop.insert(pos);
                continue;
            }
            let ops = &mut out[pos].ops;
            ops.retain(|(n, _)| !matches!(canonical_name(n).as_ref(), "lovel" | "hivel"));
            let (lo, hi) = (XFADE_LO.to_string(), XFADE_HI.to_string());
            if layer == soft {
                soft_keys.insert((p.lokey, p.hikey));
                ops.extend([("xfout_locc1".to_owned(), lo), ("xfout_hicc1".to_owned(), hi)]);
            } else {
                loud_keys.insert((p.lokey, p.hikey));
                ops.extend([("xfin_locc1".to_owned(), lo), ("xfin_hicc1".to_owned(), hi)]);
            }
        }
        let mut k = 0;
        out.retain(|_| {
            k += 1;
            !drop.contains(&(k - 1))
        });
        if soft == loud {
            self.warn(
                inst.path(),
                format!(
                    "wrap = \"strings\": the sample names show one dynamic layer ({soft}), so every region fades out as CC1 \
                     rises from {XFADE_LO} to {XFADE_HI} and nothing fades in (silent at CC1 >= {XFADE_HI}); consider \
                     wrap = \"expr\""
                ),
            );
        } else {
            let alone = soft_keys.difference(&loud_keys).count();
            if alone > 0 {
                self.warn(
                    inst.path(),
                    format!(
                        "wrap = \"strings\": {alone} key range(s) have only the soft layer ({soft}); they fade out as CC1 \
                         rises to {XFADE_HI} with no loud layer to replace them"
                    ),
                );
            }
        }
        Ok(())
    }
}

/// `prefix` + the source's `default_path` (absolute paths stay as they are).
fn relocated(prefix: &str, dp: &str) -> String {
    let b = dp.as_bytes();
    let absolute = dp.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
    if absolute { dp.to_owned() } else { format!("{prefix}{dp}") }
}

fn wrap_name(w: Wrap) -> &'static str {
    match w {
        Wrap::None => "none",
        Wrap::Expr => "expr",
        Wrap::Strings => "strings",
        Wrap::Keyswitch => "keyswitch",
        Wrap::Patch => "patch",
    }
}

/// The wrapper text: the comment line, then each block as its header and one opcode per line, blocks separated
/// by an empty line (`leading_blank` puts an empty line after the comment).
fn emit(comment: &str, leading_blank: bool, blocks: &[OutBlock]) -> String {
    let mut lines: Vec<String> = vec![];
    if leading_blank {
        lines.push(String::new());
    }
    for b in blocks {
        lines.push(format!("<{}>", b.header));
        lines.extend(b.ops.iter().map(|(k, v)| format!("{k}={v}")));
        lines.push(String::new());
    }
    format!("{comment}\n{}", lines.join("\n"))
}

fn output_file(path: PathBuf, source: PathBuf, text: &str) -> OutputFile {
    let (document, _) = Document::parse(text, &path);
    OutputFile { path, source, document }
}

/// The recorded dynamic layer in a sample name: `_v3` -> 3, else `_pp`..`_ff` followed by `_` or `.` -> 1..6.
pub fn dynamic_layer(sample: &str) -> Option<u8> {
    let b = sample.as_bytes();
    if let Some(i) = (0..b.len().saturating_sub(2)).find(|&i| b[i] == b'_' && b[i + 1] == b'v' && b[i + 2].is_ascii_digit()) {
        return Some(b[i + 2] - b'0');
    }
    const LETTERS: [(&str, u8); 6] = [("pp", 1), ("p", 2), ("mp", 3), ("mf", 4), ("f", 5), ("ff", 6)];
    for i in 0..b.len() {
        if b[i] != b'_' {
            continue;
        }
        for (name, layer) in LETTERS {
            let end = i + 1 + name.len();
            if sample.get(i + 1..end) == Some(name) && matches!(b.get(end), Some(b'_' | b'.')) {
                return Some(layer);
            }
        }
    }
    None
}

/// Does a patch operation's opcode name match (case-insensitively, aliases included)?
fn name_matches(pattern: &str, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    if pattern.contains('*') {
        return glob(&pattern, &name);
    }
    name == pattern || canonical_name(&name) == canonical_name(&pattern)
}

/// `*` matches any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(p) => rest = &rest[p + part.len()..],
                None => return false,
            }
        }
    }
    rest.is_empty()
}

fn apply_patch_op(op: &PatchOp, name: &str, value: &str) -> OpcodeEdit {
    let value_ok = |v: &Option<String>| v.as_deref().is_none_or(|v| v.trim() == value.trim());
    match op {
        PatchOp::Replace { opcode, value: v, with } if name_matches(opcode, name) && value_ok(v) => OpcodeEdit::Set(with.clone()),
        PatchOp::InsertAfter { opcode, value: v, opcodes } if name_matches(opcode, name) && value_ok(v) => {
            OpcodeEdit::InsertAfter(opcodes.0.clone())
        }
        PatchOp::Remove { opcodes } if opcodes.iter().any(|p| name_matches(p, name)) => OpcodeEdit::Remove,
        _ => OpcodeEdit::Keep,
    }
}

fn describe_op(op: &PatchOp) -> String {
    match op {
        PatchOp::Replace { opcode, value, .. } => format!("replace {opcode}{}", value.as_ref().map_or(String::new(), |v| format!("={v}"))),
        PatchOp::InsertAfter { opcode, value, .. } => {
            format!("insert after {opcode}{}", value.as_ref().map_or(String::new(), |v| format!("={v}")))
        }
        PatchOp::Remove { opcodes } => format!("remove {}", opcodes.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_from_sample_names() {
        assert_eq!(dynamic_layer("VlnEns_susVib_A2_v1.wav"), Some(1));
        assert_eq!(dynamic_layer("x/Cello_C2_v3_rr1.wav"), Some(3));
        assert_eq!(dynamic_layer("Flute_A4_ff.wav"), Some(6));
        assert_eq!(dynamic_layer("Flute_A4_mf_2.wav"), Some(4));
        assert_eq!(dynamic_layer("Flute_A4_p.flac"), Some(2));
        assert_eq!(dynamic_layer("Flute_A4_pf.wav"), None);
        assert_eq!(dynamic_layer("plain.wav"), None);
        assert_eq!(dynamic_layer("_v"), None);
    }

    #[test]
    fn globs_and_aliases() {
        assert!(name_matches("var01_*", "var01_cutoff"));
        assert!(name_matches("VAR0*", "var02_oncc120"));
        assert!(!name_matches("var01_*", "lfo01_cutoff"));
        assert!(name_matches("loop_mode", "loopmode"));
        assert!(name_matches("*_oncc1", "cutoff_oncc1"));
        assert!(!name_matches("*_oncc1", "cutoff_oncc11"));
        assert!(glob("a*b*c", "a__b__c") && !glob("a*b*c", "a__c"));
    }

    #[test]
    fn emit_matches_the_old_layout() {
        let blocks = vec![
            OutBlock::new(HeaderKind::Control, "control", pairs(&[("default_path", "x/")])),
            OutBlock::new(HeaderKind::Group, "group", vec![]),
        ];
        assert_eq!(emit("// c", true, &blocks), "// c\n\n<control>\ndefault_path=x/\n\n<group>\n");
        assert_eq!(emit("// c", false, &blocks), "// c\n<control>\ndefault_path=x/\n\n<group>\n");
    }
}
