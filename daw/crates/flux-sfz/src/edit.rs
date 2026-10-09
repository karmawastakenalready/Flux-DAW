//! Derived programs: an edited copy of an instrument that works from a new location.
//!
//! The tools in this crate never change a library in place. They make a [`Derived`] copy of an instrument,
//! edit it, and write it somewhere else. [`Derived::finish`] then points the copy back at the original samples:
//!
//! * every `<control>` block's `default_path` gets a prefix that leads from the new folder to the old one.
//!   sfizz joins `default_path` and `sample` as plain text, so adding a prefix keeps the original meaning
//!   exactly (including the odd cases, such as a `default_path` without a trailing `/`);
//! * a `<control>` block with only that prefix is added at the top when regions come before the first
//!   `<control>` (their samples were relative to the old folder);
//! * absolute `default_path`s and generator samples (`*sine`) are left alone.
//!
//! There are two layouts:
//!
//! * [`Layout::Inline`]: one self-contained file. Each `#include` is replaced by the included file's text, so
//!   comments and `#define`s stay where they were.
//! * [`Layout::Mirror`]: the root file and every included file, copied to the same relative places under the
//!   new folder, so `#include` lines keep working unchanged. Included files must be inside the root file's
//!   folder.
//!
//! Edits address what the [`Instrument`] model found (regions, opcodes) and are mapped back to the source
//! text through the locations it recorded, so everything not edited is written exactly as it was.

use crate::error::{Error, Location, Result};
use crate::fsutil::{lexical_normalize, relative_path, slash_path};
use crate::instrument::Instrument;
use crate::opcode::{HeaderKind, canonical_name};
use crate::syntax::{Document, Item, ItemKind};
use crate::text::Encoding;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How the files of a derived program are laid out. See the [module documentation](self).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// One file, includes inlined.
    Inline,
    /// The root and its included files, at the same relative places.
    Mirror,
}

/// How a derived program refers back to the original samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplePaths {
    /// Relative to the new file (`../../instruments/...`): the copy keeps working when the whole project
    /// folder moves. Falls back to absolute when there is no relative path (another drive).
    #[default]
    Relative,
    /// Absolute paths: the copy can move on its own, but not the project.
    Absolute,
}

/// A file of a derived program, ready to write.
#[derive(Clone, Debug)]
pub struct OutputFile {
    /// Where it goes (absolute).
    pub path: PathBuf,
    /// The file it was made from.
    pub source: PathBuf,
    pub document: Document,
}

impl OutputFile {
    /// The file's bytes, in the encoding of its source.
    pub fn bytes(&self) -> Vec<u8> {
        self.document.to_bytes()
    }
}

/// What to do with one opcode in [`Derived::edit_opcodes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpcodeEdit {
    Keep,
    /// Replace the value.
    Set(String),
    /// Remove the opcode (and comments after it on the same line).
    Remove,
    /// Keep it and add these opcodes after it, one per line.
    InsertAfter(Vec<(String, String)>),
}

/// An edit of one region (see [`Derived::edit_regions`]).
#[derive(Clone, Debug, PartialEq)]
pub enum RegionEdit {
    /// Give the region its own value for an opcode: the region's last opcode with that (canonical) name is
    /// changed, or a new one is added right after the `<region>` header.
    Set { name: String, value: String },
    /// Remove the region: its header, its own opcodes and comments on the same lines.
    Remove,
}

struct WorkFile {
    path: PathBuf,
    source: PathBuf,
    items: Vec<Item>,
    /// Where each item came from (`None` for items added here).
    origins: Vec<Option<Location>>,
    trailing: String,
    encoding: Encoding,
    /// The source uses CRLF line ends (added lines get them too).
    crlf: bool,
}

/// A position in the work files: (file, item).
type Pos = (usize, usize);

#[derive(Clone, Debug, PartialEq)]
enum Action {
    SetValue(String),
    Insert(Vec<Item>),
    Remove,
}

/// An edited copy of an instrument. See the [module documentation](self).
pub struct Derived<'i> {
    inst: &'i Instrument,
    out_path: PathBuf,
    out_dir: PathBuf,
    sample_paths: SamplePaths,
    files: Vec<WorkFile>,
    header_comment: Vec<String>,
    relocate_samples: bool,
}

fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

fn origin_of(file: &Path, item: &Item) -> Location {
    Location::new(file, item.line(), item.column())
}

impl<'i> Derived<'i> {
    /// Start a copy of `inst` that will be written to `out_path`. Fails if a file of the copy would replace one
    /// of the instrument's own files, or (for [`Layout::Mirror`]) if an included file is outside the root
    /// file's folder.
    pub fn new(inst: &'i Instrument, out_path: &Path, layout: Layout, sample_paths: SamplePaths) -> Result<Derived<'i>> {
        let out_path = std::path::absolute(out_path).map(|p| lexical_normalize(&p)).map_err(|e| Error::io(out_path, e))?;
        let out_dir = out_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let files = match layout {
            Layout::Inline => vec![inline(inst, &out_path)?],
            Layout::Mirror => mirror(inst, &out_path, &out_dir)?,
        };
        for f in &files {
            if let Some(src) = inst.files().iter().find(|s| same_path(&s.path, &f.path)) {
                return Err(Error::Invalid(format!(
                    "refusing to overwrite {}: derived files never replace the files they are made from",
                    src.path.display()
                )));
            }
        }
        Ok(Derived { inst, out_path, out_dir, sample_paths, files, header_comment: vec![], relocate_samples: true })
    }

    /// The instrument the copy is made from.
    pub fn instrument(&self) -> &'i Instrument {
        self.inst
    }

    /// Where the root file goes.
    pub fn out_path(&self) -> &Path {
        &self.out_path
    }

    /// Leave `default_path` and the sample paths exactly as written instead of pointing them back at the
    /// original folder. For fragments (files of regions meant to be `#include`d by a program that sets
    /// `default_path`), where a `<control>` would break the including program.
    pub fn keep_sample_paths(&mut self) {
        self.relocate_samples = false;
    }

    /// The source as a provenance comment names it: its path from the output folder (`/` separators), or the
    /// absolute path when there is none.
    pub fn source_label(&self) -> String {
        relative_path(&self.out_dir, self.inst.path()).unwrap_or_else(|| slash_path(self.inst.path()))
    }

    /// Put `// ` comment lines at the top of the root file (provenance: what made the file and from what).
    pub fn set_header_comment(&mut self, lines: &[String]) {
        self.header_comment = lines.to_vec();
    }

    /// Visit every opcode of the source text (names and values as written: variables are not expanded) and
    /// edit it. Returns how many opcodes were changed, removed or followed by insertions.
    pub fn edit_opcodes(&mut self, mut f: impl FnMut(&str, &str) -> OpcodeEdit) -> usize {
        let mut changed = 0;
        for file in &mut self.files {
            let mut i = 0;
            while i < file.items.len() {
                let edit = match (file.origins[i].is_some(), file.items[i].as_opcode()) {
                    (true, Some((name, value))) => f(name, value),
                    _ => OpcodeEdit::Keep,
                };
                match edit {
                    OpcodeEdit::Keep => i += 1,
                    OpcodeEdit::Set(v) => {
                        file.items[i].set_value(v);
                        changed += 1;
                        i += 1;
                    }
                    OpcodeEdit::Remove => {
                        remove_item(file, i);
                        changed += 1;
                    }
                    OpcodeEdit::InsertAfter(ops) => {
                        let n = ops.len();
                        for (k, (name, value)) in ops.into_iter().enumerate() {
                            file.items.insert(i + 1 + k, Item::opcode(name, value));
                            file.origins.insert(i + 1 + k, None);
                        }
                        changed += 1;
                        i += 1 + n;
                    }
                }
            }
        }
        changed
    }

    /// Edit regions, given by their index in [`Instrument::regions`]. Fails if an edit cannot be placed (a
    /// region from a file included several times, in the mirror layout, with different edits).
    pub fn edit_regions(&mut self, edits: &[(usize, RegionEdit)]) -> Result<()> {
        let index = self.index();
        let occurrence = self.region_occurrences();
        // the edits of each region, in the order given
        let mut per_region: Vec<(usize, Vec<&RegionEdit>)> = vec![];
        for (r, edit) in edits {
            match per_region.iter_mut().find(|(x, _)| x == r) {
                Some((_, list)) => list.push(edit),
                None => per_region.push((*r, vec![edit])),
            }
        }
        let mut claimed: HashMap<Pos, usize> = HashMap::new();
        let mut actions: Vec<(Pos, Action)> = vec![];
        for (k, (r, list)) in per_region.iter().enumerate() {
            let region = self.inst.regions().get(*r).ok_or_else(|| Error::Invalid(format!("no region {r}")))?;
            let occ = occurrence[*r];
            let pos_of = |loc: &Location| -> Result<Pos> {
                let list = index.get(loc).ok_or_else(|| Error::Invalid(format!("{loc}: not found in the copy")))?;
                Ok(list.get(occ).copied().unwrap_or(list[0]))
            };
            let header = pos_of(region.location())?;
            // in the mirror layout, regions of a file included twice share their text
            if let Some(&other) = claimed.get(&header) {
                if per_region[other].1 == *list {
                    continue;
                }
                return Err(Error::Invalid(format!(
                    "{}: regions {} and {r} come from the same text (the file is included more than once) and need \
                     different edits; use the inline layout",
                    region.location(),
                    per_region[other].0
                )));
            }
            claimed.insert(header, k);
            for edit in list {
                match edit {
                    RegionEdit::Set { name, value } => {
                        let canonical = canonical_name(name);
                        match region.own_opcodes().iter().rev().find(|o| o.canonical == canonical) {
                            Some(o) => actions.push((pos_of(&o.location)?, Action::SetValue(value.clone()))),
                            None => {
                                actions.push((header, Action::Insert(vec![Item::opcode(name.clone(), value.clone())])))
                            }
                        }
                    }
                    RegionEdit::Remove => {
                        actions.push((header, Action::Remove));
                        for o in region.own_opcodes() {
                            actions.push((pos_of(&o.location)?, Action::Remove));
                        }
                    }
                }
            }
        }
        self.apply(actions)
    }

    /// Point the copy back at the original samples and return its files. See the [module documentation](self).
    pub fn finish(mut self) -> Result<Vec<OutputFile>> {
        let prefix = if self.relocate_samples { self.sample_prefix() } else { String::new() };
        if !prefix.is_empty() {
            self.relocate(&prefix)?;
        }
        if !self.header_comment.is_empty() {
            let root = &mut self.files[0];
            let mut new: Vec<Item> = vec![];
            for (k, line) in self.header_comment.iter().enumerate() {
                let text = if line.is_empty() { "//".to_owned() } else { format!("// {line}") };
                new.push(Item::comment(text).with_lead(if k == 0 { "" } else { "\n" }));
            }
            let nl = if root.crlf { "\r\n" } else { "\n" };
            if let Some(first) = root.items.first_mut() {
                let lead = format!("{nl}{}", first.lead());
                first.set_lead(lead);
            } else {
                root.trailing = format!("{nl}{}", root.trailing);
            }
            let n = new.len();
            root.items.splice(0..0, new);
            root.origins.splice(0..0, std::iter::repeat_n(None, n));
        }
        Ok(self
            .files
            .into_iter()
            .map(|mut f| {
                separate_items(&mut f.items);
                if f.crlf {
                    // added lines follow the file's line ends
                    for (item, origin) in f.items.iter_mut().zip(&f.origins) {
                        if origin.is_none() && !item.lead().contains('\r') {
                            let lead = item.lead().replace('\n', "\r\n");
                            item.set_lead(lead);
                        }
                    }
                }
                let mut doc = Document::new();
                *doc.items_mut() = f.items;
                doc.set_trailing(f.trailing);
                doc.set_encoding(f.encoding);
                OutputFile { path: f.path, source: f.source, document: doc }
            })
            .collect())
    }

    /// The text to put in front of every `default_path` (empty when the copy stays in the same folder).
    fn sample_prefix(&self) -> String {
        let src_dir = self.inst.dir();
        if same_path(src_dir, &self.out_dir) {
            return String::new();
        }
        let absolute = || format!("{}/", slash_path(src_dir).trim_end_matches('/'));
        match self.sample_paths {
            SamplePaths::Absolute => absolute(),
            SamplePaths::Relative => match relative_path(&self.out_dir, src_dir) {
                Some(rel) if rel.is_empty() => String::new(),
                Some(rel) => format!("{rel}/"),
                None => absolute(),
            },
        }
    }

    fn relocate(&mut self, prefix: &str) -> Result<()> {
        let inst = self.inst;
        // a region whose sample is absolute only works while default_path is empty
        for r in inst.regions() {
            if r.default_path().is_empty()
                && let Some(o) = r.get("sample")
                && is_absolute_text(o.value.trim())
            {
                return Err(Error::Sfz {
                    location: o.location.clone(),
                    message: "the sample path is absolute and default_path is empty; a copy in another folder \
                              needs a default_path, which would break this path"
                        .into(),
                });
            }
        }
        let index = self.index();
        let mut actions: Vec<(Pos, Action)> = vec![];
        let mut seen: HashMap<&Location, usize> = HashMap::new();
        let mut first_control = None;
        let mut first_region = None;
        for (b, block) in inst.blocks().iter().enumerate() {
            match block.kind {
                HeaderKind::Control => first_control = first_control.or(Some(b)),
                HeaderKind::Region => first_region = first_region.or(Some(b)),
                _ => {}
            }
            if block.kind != HeaderKind::Control {
                continue;
            }
            let occ = {
                let c = seen.entry(&block.location).or_insert(0);
                *c += 1;
                *c - 1
            };
            let pos_of = |loc: &Location| -> Result<Pos> {
                let list = index.get(loc).ok_or_else(|| Error::Invalid(format!("{loc}: not found in the copy")))?;
                Ok(list.get(occ).copied().unwrap_or(list[0]))
            };
            let paths: Vec<_> = block.opcodes.iter().filter(|o| o.canonical == "default_path").collect();
            if paths.is_empty() {
                let item = Item::opcode("default_path", prefix.to_owned());
                actions.push((pos_of(&block.location)?, Action::Insert(vec![item])));
            }
            for o in paths {
                let value = o.value.replace('\\', "/");
                if !is_absolute_text(&value) {
                    actions.push((pos_of(&o.location)?, Action::SetValue(format!("{prefix}{value}"))));
                }
            }
        }
        self.apply(actions)?;
        let needs_top = match (first_region, first_control) {
            (Some(r), Some(c)) => r < c,
            (Some(_), None) => true,
            _ => false,
        };
        if needs_top {
            let root = &mut self.files[0];
            let items = [Item::header("control").with_lead(""), Item::opcode("default_path", prefix.to_owned())];
            let nl = if root.crlf { "\r\n" } else { "\n" };
            if let Some(first) = root.items.first_mut()
                && !first.lead().contains('\n')
            {
                let lead = format!("{nl}{}", first.lead());
                first.set_lead(lead);
            }
            root.items.splice(0..0, items);
            root.origins.splice(0..0, [None, None]);
        }
        Ok(())
    }

    /// Source location -> positions of the items made from it, in reading order.
    fn index(&self) -> HashMap<Location, Vec<Pos>> {
        let mut index: HashMap<Location, Vec<Pos>> = HashMap::new();
        for (f, file) in self.files.iter().enumerate() {
            for (i, o) in file.origins.iter().enumerate() {
                if let Some(o) = o {
                    index.entry(o.clone()).or_default().push((f, i));
                }
            }
        }
        index
    }

    /// For each region, how many earlier regions share its header location (a file included twice).
    fn region_occurrences(&self) -> Vec<usize> {
        let mut seen: HashMap<&Location, usize> = HashMap::new();
        self.inst
            .regions()
            .iter()
            .map(|r| {
                let c = seen.entry(r.location()).or_insert(0);
                *c += 1;
                *c - 1
            })
            .collect()
    }

    /// Apply edits found through [`Derived::index`] (positions are taken before anything moves).
    fn apply(&mut self, actions: Vec<(Pos, Action)>) -> Result<()> {
        let mut by_pos: HashMap<Pos, Vec<Action>> = HashMap::new();
        for (pos, action) in actions {
            let list = by_pos.entry(pos).or_default();
            if !list.contains(&action) {
                list.push(action);
            }
        }
        let mut positions: Vec<Pos> = by_pos.keys().copied().collect();
        // back to front, so earlier positions stay valid
        positions.sort_unstable_by(|a, b| b.cmp(a));
        for pos in positions {
            let actions = &by_pos[&pos];
            let sets: Vec<&String> =
                actions.iter().filter_map(|a| if let Action::SetValue(v) = a { Some(v) } else { None }).collect();
            if sets.len() > 1 {
                let loc = self.files[pos.0].origins[pos.1].clone().map_or_else(String::new, |l| l.to_string());
                return Err(Error::Invalid(format!(
                    "{loc}: different edits of the same text (the file is included more than once); use the inline \
                     layout"
                )));
            }
            let file = &mut self.files[pos.0];
            let inserts: Vec<Item> =
                actions.iter().filter_map(|a| if let Action::Insert(v) = a { Some(v.clone()) } else { None }).flatten().collect();
            if !inserts.is_empty() {
                let n = inserts.len();
                file.items.splice(pos.1 + 1..pos.1 + 1, inserts);
                file.origins.splice(pos.1 + 1..pos.1 + 1, std::iter::repeat_n(None, n));
            }
            if let Some(v) = sets.first() {
                file.items[pos.1].set_value((*v).clone());
            }
            if actions.contains(&Action::Remove) {
                remove_item(file, pos.1);
            }
        }
        Ok(())
    }
}

/// `C:/x`, `C:\x`, `/x`, `\\server\x`: sfizz takes such a sample path as is.
fn is_absolute_text(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/') || s.starts_with('\\') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

fn remove_item(file: &mut WorkFile, i: usize) {
    let mut doc = Document::new();
    *doc.items_mut() = std::mem::take(&mut file.items);
    let n = doc.remove_with_trailing_comments(i);
    file.items = std::mem::take(doc.items_mut());
    file.origins.drain(i..i + n);
}

/// Make sure items written one after the other stay separate: an item with no whitespace in front gets a
/// newline (two opcodes would otherwise merge), and nothing may follow a `//` comment on its line.
fn separate_items(items: &mut [Item]) {
    for i in 1..items.len() {
        let after_line_comment = matches!(items[i - 1].kind(), ItemKind::Comment(c) if c.starts_with("//"));
        let lead = items[i].lead();
        if lead.is_empty() || (after_line_comment && !lead.contains(['\n', '\r'])) {
            let lead = format!("\n{lead}");
            items[i].set_lead(lead);
        }
    }
}

fn uses_crlf(doc: &Document) -> bool {
    doc.items().iter().any(|i| i.lead().contains("\r\n")) || doc.trailing().contains("\r\n")
}

/// One file with every include inlined.
fn inline(inst: &Instrument, out_path: &Path) -> Result<WorkFile> {
    let root = &inst.files()[0];
    let mut file = WorkFile {
        path: out_path.to_path_buf(),
        source: root.path.clone(),
        items: vec![],
        origins: vec![],
        trailing: root.document.trailing().to_owned(),
        encoding: root.document.encoding(),
        crlf: uses_crlf(&root.document),
    };
    let mut includes = inst.includes().iter();
    inline_into(inst, &root.path, &root.document, &mut includes, &mut file)?;
    if inst.files().iter().any(|f| f.document.encoding() != file.encoding) {
        file.encoding = Encoding::Utf8;
    }
    Ok(file)
}

fn inline_into<'a>(
    inst: &'a Instrument,
    path: &Path,
    doc: &Document,
    includes: &mut std::slice::Iter<'a, crate::instrument::Include>,
    out: &mut WorkFile,
) -> Result<()> {
    for item in doc.items() {
        let loc = origin_of(path, item);
        let ItemKind::Include(raw) = item.kind() else {
            out.items.push(item.clone());
            out.origins.push(Some(loc));
            continue;
        };
        let edge = includes.next().filter(|e| e.location == loc).ok_or_else(|| {
            Error::Invalid(format!("{loc}: the include does not match the loaded instrument (was the file changed?)"))
        })?;
        let target = edge.target.as_ref().and_then(|t| inst.file(t));
        match target {
            Some(src) => {
                out.items.push(Item::comment(format!("/* #include \"{raw}\" (inlined) */")).with_lead(item.lead()));
                out.origins.push(None);
                inline_into(inst, &src.path, &src.document, includes, out)?;
            }
            None => {
                out.items.push(Item::comment(format!("/* #include \"{raw}\" was not read; left out */")).with_lead(item.lead()));
                out.origins.push(None);
            }
        }
    }
    Ok(())
}

/// The root and its included files at the same relative places under `out_dir`.
fn mirror(inst: &Instrument, out_path: &Path, out_dir: &Path) -> Result<Vec<WorkFile>> {
    let src_dir = inst.dir();
    let mut files = vec![];
    for (k, src) in inst.files().iter().enumerate() {
        let path = if k == 0 {
            out_path.to_path_buf()
        } else {
            let rel = src.path.strip_prefix(src_dir).map_err(|_| {
                Error::Invalid(format!(
                    "{} is outside the program folder {}; use the inline layout",
                    src.path.display(),
                    src_dir.display()
                ))
            })?;
            out_dir.join(rel)
        };
        let items = src.document.items().to_vec();
        let origins = items.iter().map(|i| Some(origin_of(&src.path, i))).collect();
        files.push(WorkFile {
            path,
            source: src.path.clone(),
            items,
            origins,
            trailing: src.document.trailing().to_owned(),
            encoding: src.document.encoding(),
            crlf: uses_crlf(&src.document),
        });
    }
    Ok(files)
}

/// Write the files of a derived program (creating folders as needed).
pub fn write_files(files: &[OutputFile]) -> Result<()> {
    for f in files {
        crate::error::write_file(&f.path, &f.bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::LoadOptions;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flux-sfz-edit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts() -> LoadOptions {
        LoadOptions { check_samples: false, ..LoadOptions::default() }
    }

    #[test]
    fn relocation_prefixes_default_path() {
        let d = tmp("reloc");
        std::fs::create_dir_all(d.join("lib/Programs")).unwrap();
        let src = d.join("lib/Programs/p.sfz");
        std::fs::write(&src, "<control>\ndefault_path=../Samples/\n<region> sample=a.wav\n<control>\n<region> sample=b.wav\n").unwrap();
        let inst = Instrument::load_with(&src, &opts()).unwrap();
        let out = d.join("out/p.sfz");
        let files = Derived::new(&inst, &out, Layout::Inline, SamplePaths::Relative).unwrap().finish().unwrap();
        let text = files[0].document.to_text();
        assert_eq!(
            text,
            "<control>\ndefault_path=../lib/Programs/../Samples/\n<region> sample=a.wav\n<control>\ndefault_path=../lib/Programs/\n<region> sample=b.wav\n"
        );
        // the copy plays the same files
        let copy = Instrument::from_text(&text, &out, &opts()).unwrap();
        let paths = |i: &Instrument| -> Vec<PathBuf> { i.regions().iter().map(|r| r.sample().file().unwrap().path.clone()).collect() };
        assert_eq!(paths(&copy), paths(&inst));
        let abs = Derived::new(&inst, &out, Layout::Inline, SamplePaths::Absolute).unwrap().finish().unwrap();
        let copy = Instrument::from_text(&abs[0].document.to_text(), &out, &opts()).unwrap();
        assert_eq!(paths(&copy), paths(&inst));
        // refuses to overwrite its source
        assert!(Derived::new(&inst, &src, Layout::Inline, SamplePaths::Relative).is_err());
    }

    #[test]
    fn regions_before_any_control_get_one() {
        let d = tmp("top");
        let src = d.join("a/p.sfz");
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(&src, "// c\n<region> sample=x.wav\n").unwrap();
        let inst = Instrument::load_with(&src, &opts()).unwrap();
        let mut der = Derived::new(&inst, &d.join("b/p.sfz"), Layout::Inline, SamplePaths::Relative).unwrap();
        der.set_header_comment(&["made by a test".into()]);
        let files = der.finish().unwrap();
        assert_eq!(files[0].document.to_text(), "// made by a test\n<control>\ndefault_path=../a/\n// c\n<region> sample=x.wav\n");
    }

    #[test]
    fn includes_inline_or_mirror() {
        let d = tmp("inc");
        std::fs::create_dir_all(d.join("p/m")).unwrap();
        std::fs::write(d.join("p/m/map.sfz"), "<region> sample=s.wav key=60 // one\n").unwrap();
        std::fs::write(d.join("p/root.sfz"), "#define $V -3\n<group> volume=$V\n#include \"m/map.sfz\"\n#include \"m/map.sfz\"\n").unwrap();
        let inst = Instrument::load_with(d.join("p/root.sfz"), &opts()).unwrap();
        assert_eq!(inst.regions().len(), 2);
        let mut der = Derived::new(&inst, &d.join("q/root.sfz"), Layout::Inline, SamplePaths::Relative).unwrap();
        der.edit_regions(&[(1, RegionEdit::Set { name: "tune".into(), value: "5".into() })]).unwrap();
        let files = der.finish().unwrap();
        assert_eq!(files.len(), 1);
        let text = files[0].document.to_text();
        assert_eq!(
            text,
            "<control>\ndefault_path=../p/\n#define $V -3\n<group> volume=$V\n/* #include \"m/map.sfz\" (inlined) */\
             \n<region> sample=s.wav key=60 // one\n/* #include \"m/map.sfz\" (inlined) */\n<region>\ntune=5 sample=s.wav \
             key=60 // one\n"
        );
        let copy = Instrument::from_text(&text, d.join("q/root.sfz"), &opts()).unwrap();
        assert_eq!(copy.regions()[1].props().tune, 5.0);
        assert_eq!(copy.regions()[0].props().volume, -3.0);
        // mirror: the shared region can take one edit, not two different ones
        let set = |v: &str| RegionEdit::Set { name: "tune".into(), value: v.into() };
        let mut der = Derived::new(&inst, &d.join("q/root.sfz"), Layout::Mirror, SamplePaths::Relative).unwrap();
        assert!(der.edit_regions(&[(0, set("1")), (1, set("2"))]).is_err());
        der.edit_regions(&[(0, set("1")), (1, set("1"))]).unwrap();
        let files = der.finish().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].path, lexical_normalize(&d.join("q/m/map.sfz")));
        assert_eq!(files[1].document.to_text(), "<region>\ntune=1 sample=s.wav key=60 // one\n");
        assert_eq!(files[0].document.to_text(), "<control>\ndefault_path=../p/\n#define $V -3\n<group> volume=$V\n#include \"m/map.sfz\"\n#include \"m/map.sfz\"\n");
    }

    #[test]
    fn opcode_and_region_edits_keep_the_rest() {
        let d = tmp("ops");
        let src = d.join("p.sfz");
        std::fs::write(
            &src,
            "<control>\n<global>\nloop_mode=continuous\ncutoff=250\nvar01_x=1 //gone\nvar02_y=2\nvol=1\n<region> sample=*sine tune=3\n<region> sample=*saw\n",
        )
        .unwrap();
        let inst = Instrument::load_with(&src, &opts()).unwrap();
        let mut der = Derived::new(&inst, &d.join("sub/p.sfz"), Layout::Mirror, SamplePaths::Relative).unwrap();
        let n = der.edit_opcodes(|name, value| match (name, value) {
            ("loop_mode", "continuous") => OpcodeEdit::Set("loop_continuous".into()),
            ("cutoff", "250") => OpcodeEdit::InsertAfter(vec![("cutoff_oncc120".into(), "6000".into())]),
            (n, _) if n.starts_with("var0") => OpcodeEdit::Remove,
            _ => OpcodeEdit::Keep,
        });
        assert_eq!(n, 4);
        der.edit_regions(&[
            (0, RegionEdit::Set { name: "pitch".into(), value: "-7".into() }),
            (1, RegionEdit::Remove),
        ])
        .unwrap();
        let text = der.finish().unwrap()[0].document.to_text();
        assert_eq!(
            text,
            "<control>\ndefault_path=../\n<global>\nloop_mode=loop_continuous\ncutoff=250\ncutoff_oncc120=6000\nvol=1\n<region> sample=*sine tune=-7\n"
        );
    }
}
