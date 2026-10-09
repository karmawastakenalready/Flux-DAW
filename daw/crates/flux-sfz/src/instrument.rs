//! The instrument as sfizz plays it: includes followed, variables expanded, headers inherited, every region
//! resolved to its effective opcodes and its sample file.
//!
//! The rules (each checked against sfizz 1.2.3, see `build/flux-sfz_check/probe/`):
//!
//! * `#include "path"` is resolved relative to the folder of the file that was loaded (the *root* file), not
//!   the including file; backslashes are separators. The included text continues the current header, so an
//!   include in the middle of a `<group>` adds opcodes to that group. A missing file is an error and is
//!   skipped. sfizz expands an include cycle until its depth limit of 32; this loader reports the cycle and
//!   does not follow it.
//! * `#define $NAME value` applies to the text after it, in this file and in everything included later.
//!   `$NAME` is replaced in opcode names, values and include paths; the shortest defined name wins (`$K0`
//!   with `$K` defined reads as `$K` followed by `0`) and the replacement is read again, so variables may
//!   refer to other variables (using their value at the point of use). An undefined variable is removed
//!   with a warning. A variable that refers to itself is an error; sfizz 1.2.3 hangs on such a file.
//! * `<global>` replaces the global opcodes and clears master and group; `<master>` replaces the master
//!   opcodes and clears the group; `<group>` replaces the group opcodes. A region applies global, master,
//!   group and its own opcodes in that order; later ones win.
//! * `<control>` resets `default_path` to empty. A region's sample is `default_path` + `sample` as plain
//!   text (`default_path=sub` and `sample=a.wav` give `suba.wav`), backslashes become `/`, and a relative
//!   result is relative to the root file's folder. Generators (`*sine`) do not use `default_path`. sfizz
//!   removes regions whose sample file does not exist; they are kept here with [`FileStatus::Missing`].
//! * Opcode names are case-insensitive; header names are not (`<Region>` is unknown and ignored with its
//!   opcodes). Opcodes before the first header are ignored.
//! * `octave_offset` and `note_offset` in `<control>` shift every key opcode read after them.

use crate::error::{Diagnostic, Error, Location, Result, Severity};
use crate::fsutil::{FileChecker, FileStatus, lexical_normalize, slash_path};
use crate::opcode::{HeaderKind, NamePattern, Support, canonical_name, support};
use crate::props::RegionProps;
use crate::syntax::{Document, ItemKind, is_ident_char};
use crate::value::{leading_float, leading_int};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How to load an instrument.
#[derive(Clone, Debug)]
pub struct LoadOptions {
    /// Variables defined before the file is read (`("NAME", "value")`, without the `$`).
    pub defines: Vec<(String, String)>,
    /// How deep `#include`s may nest (sfizz: 32).
    pub max_include_depth: usize,
    /// Look every sample file up on disk (missing files, case mismatches). On by default.
    pub check_samples: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions { defines: vec![], max_include_depth: 32, check_samples: true }
    }
}

/// An opcode after variable expansion.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Opcode {
    /// The name as written (variables expanded).
    pub name: String,
    /// The name sfizz uses: lower-case, aliases resolved (see [`crate::opcode::canonical_name`]).
    pub canonical: String,
    pub value: String,
    pub location: Location,
}

impl Opcode {
    pub fn new(name: impl Into<String>, value: impl Into<String>, location: Location) -> Opcode {
        let name = name.into();
        let canonical = canonical_name(&name).into_owned();
        Opcode { name, canonical, value: value.into(), location }
    }
}

/// A header and its opcodes, after includes and variables. Blocks are listed in file order with included
/// files inlined.
#[derive(Clone, Debug, Serialize)]
pub struct Block {
    pub kind: HeaderKind,
    /// The header name as written (meaningful for [`HeaderKind::Unknown`]).
    pub name: String,
    pub location: Location,
    pub opcodes: Arc<[Opcode]>,
}

/// Where a region's opcode comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Global,
    Master,
    Group,
    Region,
}

/// What a region plays.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SampleRef {
    /// No `sample` opcode (sfizz keeps the region; it plays nothing).
    None,
    /// A built-in generator such as `*sine` or `*silence`.
    Generator { name: String },
    File(SampleFile),
}

/// A sample file referenced by a region.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SampleFile {
    /// `default_path` + `sample` with `\` turned into `/`: the text sfizz looks up.
    pub value: String,
    /// The absolute path (`.` and `..` resolved).
    pub path: PathBuf,
    pub status: FileStatus,
}

impl SampleRef {
    pub fn file(&self) -> Option<&SampleFile> {
        match self {
            SampleRef::File(f) => Some(f),
            _ => None,
        }
    }
}

/// One region with everything it inherits.
#[derive(Clone, Debug)]
pub struct Region {
    index: usize,
    location: Location,
    /// global, master, group, own
    levels: [Arc<[Opcode]>; 4],
    default_path: String,
    sample: SampleRef,
    props: RegionProps,
}

const LEVELS: [Level; 4] = [Level::Global, Level::Master, Level::Group, Level::Region];

impl Region {
    /// Position among all regions of the instrument (0-based, file order). sfizz numbers the regions it keeps,
    /// so the numbers differ once a region with a missing sample has been dropped.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The `<region>` header.
    pub fn location(&self) -> &Location {
        &self.location
    }

    /// Every opcode that applies to the region, in the order sfizz applies them (overridden ones included).
    pub fn opcodes(&self) -> impl Iterator<Item = (Level, &Opcode)> {
        LEVELS.iter().zip(&self.levels).flat_map(|(&l, ops)| ops.iter().map(move |o| (l, o)))
    }

    /// The opcodes written in the region itself.
    pub fn own_opcodes(&self) -> &[Opcode] {
        &self.levels[3]
    }

    /// The opcodes inherited from one level.
    pub fn level_opcodes(&self, level: Level) -> &[Opcode] {
        &self.levels[level as usize]
    }

    /// The opcode in effect for a name (canonical names, so `loopmode` finds `loop_mode`): the last one applied.
    pub fn get(&self, name: &str) -> Option<&Opcode> {
        let name = canonical_name(name);
        self.levels.iter().rev().flat_map(|ops| ops.iter().rev()).find(|o| o.canonical == name)
    }

    /// The value in effect for an opcode name.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.get(name).map(|o| o.value.as_str())
    }

    /// The effective opcodes: for each canonical name the opcode that wins, in application order.
    pub fn effective(&self) -> Vec<(Level, &Opcode)> {
        let mut last: HashMap<&str, usize> = HashMap::new();
        let all: Vec<(Level, &Opcode)> = self.opcodes().collect();
        for (i, (_, o)) in all.iter().enumerate() {
            last.insert(o.canonical.as_str(), i);
        }
        all.iter().enumerate().filter(|(i, (_, o))| last[o.canonical.as_str()] == *i).map(|(_, x)| *x).collect()
    }

    /// The `default_path` in effect when the region was read.
    pub fn default_path(&self) -> &str {
        &self.default_path
    }

    pub fn sample(&self) -> &SampleRef {
        &self.sample
    }

    /// The interpreted opcodes (keys, velocities, keyswitches, levels...).
    pub fn props(&self) -> &RegionProps {
        &self.props
    }

    /// True if sfizz keeps the region (its sample file exists, or it uses a generator or no sample).
    pub fn is_playable(&self) -> bool {
        self.sample.file().is_none_or(|f| f.status.exists())
    }
}

/// Instrument-wide settings from `<control>` blocks.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Control {
    /// Initial controller values (`set_ccN`, 0..127; `set_hdccN` is stored multiplied by 127).
    pub set_cc: BTreeMap<u32, f64>,
    /// Controller names (`label_ccN`).
    pub label_cc: BTreeMap<u32, String>,
    /// Key names (`label_keyN`).
    pub label_key: BTreeMap<u8, String>,
    pub note_offset: i32,
    pub octave_offset: i32,
    /// `hint_*` opcodes as written.
    pub hints: Vec<(String, String)>,
}

/// An `#include` directive met while reading, in reading order.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Include {
    /// Where the directive is.
    pub location: Location,
    /// The path as written, variables expanded, `\` turned into `/`.
    pub path: String,
    /// The file read (absolute), or `None` if it was not followed (missing, a cycle, too deep).
    pub target: Option<PathBuf>,
}

/// A file that was read: the root file first, then each included file once, in the order first included.
#[derive(Clone, Debug)]
pub struct SourceFile {
    pub path: PathBuf,
    /// The first `#include` that brought the file in (`None` for the root file).
    pub included_from: Option<Location>,
    pub document: Arc<Document>,
}

/// An SFZ instrument resolved the way sfizz 1.2.3 resolves it. See the [module documentation](self).
#[derive(Clone, Debug)]
pub struct Instrument {
    path: PathBuf,
    dir: PathBuf,
    files: Vec<SourceFile>,
    includes: Vec<Include>,
    blocks: Vec<Block>,
    regions: Vec<Region>,
    control: Control,
    diagnostics: Vec<Diagnostic>,
}

impl Instrument {
    /// Load an SFZ file and everything it includes. Fails only if the file itself cannot be read; problems
    /// inside it are [`Instrument::diagnostics`].
    pub fn load(path: impl AsRef<Path>) -> Result<Instrument> {
        Instrument::load_with(path, &LoadOptions::default())
    }

    pub fn load_with(path: impl AsRef<Path>, options: &LoadOptions) -> Result<Instrument> {
        let path = absolute(path.as_ref())?;
        let bytes = crate::error::read_file(&path)?;
        let (doc, diags) = Document::parse_bytes(&bytes, &path);
        Ok(Loader::new(&path, options).run(doc, diags))
    }

    /// Resolve SFZ text as if it were the file `path` (which need not exist): includes and relative samples
    /// are looked up next to it.
    pub fn from_text(text: &str, path: impl AsRef<Path>, options: &LoadOptions) -> Result<Instrument> {
        let path = absolute(path.as_ref())?;
        let (doc, diags) = Document::parse(text, &path);
        Ok(Loader::new(&path, options).run(doc, diags))
    }

    /// The root file (absolute).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The root file's folder: the base of includes and relative sample paths.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every file read (root first).
    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }

    /// The file read from `path` (absolute, normalised), if it is part of the instrument.
    pub fn file(&self, path: &Path) -> Option<&SourceFile> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Every `#include` met, in reading order (an include inside a file included twice appears twice).
    pub fn includes(&self) -> &[Include] {
        &self.includes
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// All regions in file order, including those sfizz drops because their sample is missing.
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// The regions sfizz keeps, in sfizz's numbering order.
    pub fn playable_regions(&self) -> impl Iterator<Item = &Region> {
        self.regions.iter().filter(|r| r.is_playable())
    }

    pub fn control(&self) -> &Control {
        &self.control
    }

    /// Everything found wrong or suspicious, in reading order.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The diagnostics with [`Severity::Error`].
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.severity == Severity::Error)
    }

    /// Every distinct sample file, in first-use order.
    pub fn sample_files(&self) -> Vec<&SampleFile> {
        let mut seen = HashSet::new();
        self.regions.iter().filter_map(|r| r.sample.file()).filter(|f| seen.insert(&f.path)).collect()
    }

    /// The instrument as one self-contained SFZ text: includes inlined, variables expanded, comments dropped,
    /// one opcode per line. Sample paths stay as written, so the text is valid next to the root file.
    pub fn to_flat_text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            out.push('<');
            out.push_str(&b.name);
            out.push_str(">\n");
            for o in b.opcodes.iter() {
                out.push_str(&o.name);
                out.push('=');
                out.push_str(&o.value);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map(|p| lexical_normalize(&p)).map_err(|e| Error::io(path, e))
}

// ---------------------------------------------------------------------------------------------
// variable expansion

/// At most this many substitutions in one piece of text before the expansion counts as endless.
const MAX_SUBSTITUTIONS: usize = 10_000;
const MAX_EXPANDED_LEN: usize = 1 << 20;

/// Expand `$VARIABLES` in `text` (see the module docs). Problems go to `issues` as (severity, message).
pub(crate) fn expand_variables(
    text: &str,
    defines: &HashMap<String, String>,
    issues: &mut Vec<(Severity, String)>,
) -> String {
    if !text.contains('$') {
        return text.to_owned();
    }
    let mut buf: Vec<u8> = text.as_bytes().to_vec();
    let mut out: Vec<u8> = Vec::with_capacity(buf.len());
    let mut i = 0;
    let mut substitutions = 0;
    while i < buf.len() {
        if buf[i] != b'$' {
            out.push(buf[i]);
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        let mut found: Option<&String> = None;
        while j < buf.len() && is_ident_char(buf[j]) {
            j += 1;
            // names are ASCII identifier characters, so this slice is valid UTF-8
            if let Some(v) = std::str::from_utf8(&buf[start..j]).ok().and_then(|n| defines.get(n)) {
                found = Some(v);
                break;
            }
        }
        match found {
            Some(v) => {
                substitutions += 1;
                if substitutions > MAX_SUBSTITUTIONS || buf.len() + v.len() > MAX_EXPANDED_LEN {
                    let name = String::from_utf8_lossy(&buf[start..j]).into_owned();
                    issues.push((
                        Severity::Error,
                        format!(
                            "the variable `{name}` expands without end (it refers to itself); sfizz 1.2.3 hangs on \
                             this file"
                        ),
                    ));
                    break;
                }
                // read the replacement again: variables may refer to variables
                buf.splice(i..j, v.bytes());
            }
            None if j == start => {
                issues.push((Severity::Warning, "Expected variable name after $.".into()));
                i += 1;
            }
            None => {
                let name = String::from_utf8_lossy(&buf[start..j]).into_owned();
                issues.push((Severity::Warning, format!("The variable `{name}` is not defined.")));
                i = j;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

// ---------------------------------------------------------------------------------------------
// loader

struct PendingBlock {
    kind: HeaderKind,
    name: String,
    location: Location,
    opcodes: Vec<Opcode>,
}

struct Loader<'o> {
    options: &'o LoadOptions,
    path: PathBuf,
    dir: PathBuf,
    defines: HashMap<String, String>,
    /// identity -> (document, path it was first read from)
    docs: HashMap<PathBuf, (Arc<Document>, PathBuf)>,
    files: Vec<SourceFile>,
    includes: Vec<Include>,
    /// identities of the files being read (for cycle detection)
    stack: Vec<PathBuf>,
    diags: Vec<Diagnostic>,
    reported: HashSet<(Location, String)>,
    current: Option<PendingBlock>,
    blocks: Vec<Block>,
    levels: [Arc<[Opcode]>; 3],
    default_path: String,
    control: Control,
    regions: Vec<Region>,
    checker: FileChecker,
    missing: Vec<(PathBuf, String, Location, usize)>,
}

impl<'o> Loader<'o> {
    fn new(path: &Path, options: &'o LoadOptions) -> Loader<'o> {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let empty: Arc<[Opcode]> = Arc::from(Vec::new());
        Loader {
            options,
            path: path.to_path_buf(),
            dir,
            defines: options.defines.iter().cloned().collect(),
            docs: HashMap::new(),
            files: vec![],
            includes: vec![],
            stack: vec![],
            diags: vec![],
            reported: HashSet::new(),
            current: None,
            blocks: vec![],
            levels: [empty.clone(), empty.clone(), empty],
            default_path: String::new(),
            control: Control::default(),
            regions: vec![],
            checker: FileChecker::new(),
            missing: vec![],
        }
    }

    fn run(mut self, doc: Document, parse_diags: Vec<Diagnostic>) -> Instrument {
        self.diags.extend(parse_diags);
        let doc = Arc::new(doc);
        let root = self.path.clone();
        self.files.push(SourceFile { path: root.clone(), included_from: None, document: doc.clone() });
        self.stack.push(identity(&root));
        self.walk(&doc, &root, 0);
        self.finish_block();
        for (_, value, location, count) in std::mem::take(&mut self.missing) {
            let regions = if count == 1 { "1 region".to_owned() } else { format!("{count} regions") };
            self.report(
                Severity::Error,
                location,
                format!("sample not found: {value} ({regions}; sfizz drops regions without their sample)"),
            );
        }
        Instrument {
            dir: self.dir,
            path: self.path,
            files: self.files,
            includes: self.includes,
            blocks: self.blocks,
            regions: self.regions,
            control: self.control,
            diagnostics: self.diags,
        }
    }

    fn report(&mut self, severity: Severity, location: Location, message: String) {
        if self.reported.insert((location.clone(), message.clone())) {
            self.diags.push(Diagnostic { severity, location, message });
        }
    }

    fn expand(&mut self, text: &str, location: &Location) -> String {
        if !text.contains('$') {
            return text.to_owned();
        }
        let mut issues = vec![];
        let out = expand_variables(text, &self.defines, &mut issues);
        for (severity, message) in issues {
            self.report(severity, location.clone(), message);
        }
        out
    }

    fn walk(&mut self, doc: &Document, file: &Path, depth: usize) {
        for item in doc.items() {
            let location = Location::new(file, item.line(), item.column());
            match item.kind() {
                ItemKind::Header(name) => {
                    self.finish_block();
                    let kind = HeaderKind::from_name(name);
                    if kind == HeaderKind::Unknown {
                        self.report(
                            Severity::Error,
                            location.clone(),
                            format!("unknown header <{name}>; sfizz ignores it and its opcodes (headers are case-sensitive)"),
                        );
                    }
                    self.current = Some(PendingBlock { kind, name: name.clone(), location, opcodes: vec![] });
                }
                ItemKind::Opcode { name, value } => {
                    let name = self.expand(name, &location);
                    let value = self.expand(value, &location);
                    match &mut self.current {
                        Some(b) => b.opcodes.push(Opcode::new(name, value, location)),
                        None => self.report(
                            Severity::Warning,
                            location,
                            format!("`{name}` is not under any header; sfizz ignores it"),
                        ),
                    }
                }
                ItemKind::Define { name, value } => {
                    self.defines.insert(name.clone(), value.clone());
                }
                ItemKind::Include(raw) => self.include(raw, location, depth),
                ItemKind::Comment(_) | ItemKind::Invalid(_) => {}
            }
        }
    }

    fn include(&mut self, raw: &str, location: Location, depth: usize) {
        let rel = self.expand(raw, &location).replace('\\', "/");
        let index = self.includes.len();
        self.includes.push(Include { location: location.clone(), path: rel.clone(), target: None });
        if depth + 1 >= self.options.max_include_depth {
            let max = self.options.max_include_depth;
            self.report(Severity::Error, location, format!("Exceeded maximum include depth ({max})"));
            return;
        }
        let path = lexical_normalize(&self.dir.join(&rel));
        let id = identity(&path);
        if let Some(pos) = self.stack.iter().position(|p| *p == id) {
            let chain: Vec<String> = self.stack[pos..]
                .iter()
                .chain(std::iter::once(&id))
                .map(|p| p.file_name().map_or_else(|| slash_path(p), |n| n.to_string_lossy().into_owned()))
                .collect();
            self.report(
                Severity::Error,
                location,
                format!("include cycle ({}); not followed (sfizz repeats it up to 32 times)", chain.join(" -> ")),
            );
            return;
        }
        let (doc, path) = match self.docs.get(&id) {
            Some(d) => d.clone(),
            None => match crate::error::read_file(&path) {
                Ok(bytes) => {
                    let (doc, diags) = Document::parse_bytes(&bytes, &path);
                    self.diags.extend(diags);
                    let doc = Arc::new(doc);
                    self.docs.insert(id.clone(), (doc.clone(), path.clone()));
                    self.files.push(SourceFile {
                        path: path.clone(),
                        included_from: Some(location),
                        document: doc.clone(),
                    });
                    (doc, path)
                }
                Err(e) => {
                    let why = match &e {
                        Error::Io { source, .. } => source.to_string(),
                        other => other.to_string(),
                    };
                    self.report(Severity::Error, location, format!("cannot include \"{rel}\": {why}"));
                    return;
                }
            },
        };
        self.includes[index].target = Some(path.clone());
        self.stack.push(id);
        self.walk(&doc, &path, depth + 1);
        self.stack.pop();
    }

    fn finish_block(&mut self) {
        let Some(b) = self.current.take() else { return };
        let opcodes: Arc<[Opcode]> = Arc::from(b.opcodes);
        for o in opcodes.iter() {
            if support(b.kind, &o.name) == Support::Unknown {
                let msg = match b.kind {
                    HeaderKind::Control => format!("sfizz does not support `{}` in <control>; it is ignored", o.name),
                    _ => format!("unknown opcode `{}`; sfizz ignores it", o.name),
                };
                self.report(Severity::Warning, o.location.clone(), msg);
            }
        }
        match b.kind {
            HeaderKind::Control => self.apply_control(&opcodes),
            HeaderKind::Global => {
                let empty: Arc<[Opcode]> = Arc::from(Vec::new());
                self.levels = [opcodes.clone(), empty.clone(), empty];
            }
            HeaderKind::Master => {
                self.levels[1] = opcodes.clone();
                self.levels[2] = Arc::from(Vec::new());
            }
            HeaderKind::Group => self.levels[2] = opcodes.clone(),
            HeaderKind::Region => self.make_region(opcodes.clone(), b.location.clone()),
            HeaderKind::Sample => self.report(
                Severity::Warning,
                b.location.clone(),
                "embedded <sample> data is not supported by flux-sfz; the block is ignored".into(),
            ),
            HeaderKind::Curve | HeaderKind::Effect | HeaderKind::Unknown => {}
        }
        self.blocks.push(Block { kind: b.kind, name: b.name, location: b.location, opcodes });
    }

    fn apply_control(&mut self, opcodes: &[Opcode]) {
        self.default_path.clear();
        for o in opcodes {
            let p = NamePattern::parse(&o.name);
            let n = p.numbers.first().copied();
            match (p.pattern.as_str(), n) {
                ("default_path", _) => self.default_path = o.value.replace('\\', "/"),
                ("note_offset", _) => {
                    self.control.note_offset = leading_int(&o.value).unwrap_or(0).clamp(-127, 127) as i32;
                }
                ("octave_offset", _) => {
                    self.control.octave_offset = leading_int(&o.value).unwrap_or(0).clamp(-10, 10) as i32;
                }
                ("set_ccN", Some(cc)) => {
                    if let Some(v) = leading_float(&o.value) {
                        self.control.set_cc.insert(cc, v);
                    }
                }
                ("set_hdccN" | "set_realccN", Some(cc)) => {
                    if let Some(v) = leading_float(&o.value) {
                        self.control.set_cc.insert(cc, v * 127.0);
                    }
                }
                ("label_ccN", Some(cc)) => {
                    self.control.label_cc.insert(cc, o.value.clone());
                }
                ("label_keyN", Some(k)) => {
                    if let Ok(k) = u8::try_from(k)
                        && k <= 127
                    {
                        self.control.label_key.insert(k, o.value.clone());
                    }
                }
                (name, _) if name.starts_with("hint_") => self.control.hints.push((o.name.clone(), o.value.clone())),
                _ => {}
            }
        }
    }

    fn key_shift(&self) -> i32 {
        self.control.octave_offset * 12 + self.control.note_offset
    }

    fn make_region(&mut self, own: Arc<[Opcode]>, location: Location) {
        let levels = [self.levels[0].clone(), self.levels[1].clone(), self.levels[2].clone(), own];
        let mut props = RegionProps::default();
        let mut sample: Option<Opcode> = None;
        let shift = self.key_shift();
        let mut issues = vec![];
        for o in levels.iter().flat_map(|l| l.iter()) {
            if o.canonical == "sample" {
                sample = Some(o.clone());
            }
            if let Some(issue) = props.apply(&o.canonical, &o.value, shift) {
                issues.push((o.location.clone(), issue.message));
            }
        }
        for (loc, msg) in issues {
            self.report(Severity::Warning, loc, msg);
        }
        let sample = match sample {
            None => SampleRef::None,
            Some(o) => self.resolve_sample(&o),
        };
        let index = self.regions.len();
        self.regions.push(Region { index, location, levels, default_path: self.default_path.clone(), sample, props });
    }

    fn resolve_sample(&mut self, o: &Opcode) -> SampleRef {
        let v = o.value.trim();
        if v.is_empty() {
            return SampleRef::None;
        }
        if v.starts_with('*') {
            return SampleRef::Generator { name: v.to_owned() };
        }
        let value = format!("{}{}", self.default_path, v.replace('\\', "/"));
        let path = lexical_normalize(&self.dir.join(&value));
        let status = if self.options.check_samples { self.checker.status(&path) } else { FileStatus::Unchecked };
        match &status {
            FileStatus::Missing => match self.missing.iter_mut().find(|m| m.0 == path) {
                Some(m) => m.3 += 1,
                None => self.missing.push((path.clone(), value.clone(), o.location.clone(), 1)),
            },
            FileStatus::CaseMismatch { on_disk } => {
                let msg = format!(
                    "the sample's name differs in case from the file on disk ({} vs {}); only case-insensitive \
                     filesystems find it",
                    value,
                    slash_path(on_disk)
                );
                self.report(Severity::Warning, o.location.clone(), msg);
            }
            FileStatus::Found | FileStatus::Unchecked => {}
        }
        SampleRef::File(SampleFile { value, path, status })
    }
}

/// The identity of a file for cycle detection (the canonical path when it exists).
fn identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::Trigger;

    fn load(text: &str) -> Instrument {
        let opts = LoadOptions { check_samples: false, ..LoadOptions::default() };
        Instrument::from_text(text, std::env::temp_dir().join("flux-sfz-test/virtual.sfz"), &opts).unwrap()
    }

    fn defines(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn variables_expand_like_sfizz() {
        let mut issues = vec![];
        let d = defines(&[("K", "6"), ("K0", "7")]);
        assert_eq!(expand_variables("$K0", &d, &mut issues), "60");
        let d = defines(&[("C", "6")]);
        assert_eq!(expand_variables("$C$C", &d, &mut issues), "66");
        assert!(issues.is_empty());
        // `$` without a name and undefined names are removed with a warning
        assert_eq!(expand_variables("a$.b $-x $", &d, &mut issues), "a.b -x ");
        assert_eq!(issues.len(), 3);
        issues.clear();
        assert_eq!(expand_variables("x$abc", &defines(&[("Abc", "1")]), &mut issues), "x");
        assert_eq!(issues[0].1, "The variable `abc` is not defined.");
        // replacements are read again
        let d = defines(&[("P", "$Q"), ("Q", "k"), ("D", "$"), ("E", "$Q")]);
        let mut issues = vec![];
        assert_eq!(expand_variables("$P$Q", &d, &mut issues), "kk");
        assert_eq!(expand_variables("$DQ", &d, &mut issues), "k");
        assert_eq!(expand_variables("$E2", &d, &mut issues), "k2");
        assert!(issues.is_empty());
    }

    #[test]
    fn self_reference_does_not_hang() {
        let mut issues = vec![];
        let _ = expand_variables("a$Xb", &defines(&[("X", "$X")]), &mut issues);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].0, Severity::Error);
    }

    #[test]
    fn defines_apply_at_the_point_of_use() {
        let i = load("#define $B 62\n#define $A $B\n<region> sample=*sine key=$A\n#define $B 64\n<region> sample=*sine key=$A\n");
        let keys: Vec<u8> = i.regions().iter().map(|r| r.props().lokey).collect();
        assert_eq!(keys, vec![62, 64]);
    }

    #[test]
    fn headers_inherit() {
        let i = load(
            "<global> volume=-6 <master> tune=10 <group> lokey=40 \
             <region> sample=*sine hikey=50\n<group> hikey=60 <region> sample=*saw\n<master> <region> sample=*sine\n\
             <global> <region> sample=*sine\n",
        );
        let r = i.regions();
        assert_eq!((r[0].props().volume, r[0].props().tune, r[0].props().lokey, r[0].props().hikey), (-6.0, 10.0, 40, 50));
        // a new group replaces the old one; master and global remain
        assert_eq!((r[1].props().lokey, r[1].props().hikey, r[1].props().tune), (0, 60, 10.0));
        // a new master clears the group
        assert_eq!((r[2].props().volume, r[2].props().tune, r[2].props().hikey), (-6.0, 0.0, 127));
        // a new global clears everything
        assert_eq!(r[3].props().volume, 0.0);
        assert_eq!(r[0].get("volume").map(|o| o.value.as_str()), Some("-6"));
        assert_eq!(r[0].effective().len(), 5);
    }

    #[test]
    fn default_path_is_concatenated_and_reset_by_control() {
        let i = load(
            "<control> default_path=sub\n<region> sample=a.wav\n<region> sample=*sine\n<control>\n\
             <region> sample=sub\\b.wav\n<control> default_path=..\\x/\n<region> sample=c.wav\n",
        );
        let v: Vec<String> = i
            .regions()
            .iter()
            .map(|r| match r.sample() {
                SampleRef::File(f) => f.value.clone(),
                SampleRef::Generator { name } => name.clone(),
                SampleRef::None => String::new(),
            })
            .collect();
        assert_eq!(v, vec!["suba.wav", "*sine", "sub/b.wav", "../x/c.wav"]);
        let f = i.regions()[3].sample().file().unwrap();
        assert!(f.path.ends_with("x/c.wav") || f.path.ends_with("x\\c.wav"));
        assert!(!f.path.to_string_lossy().contains(".."));
    }

    #[test]
    fn unknown_headers_and_stray_opcodes() {
        let i = load("lokey=1\n<Region> sample=*sine key=62\n<region> sample=*saw key=63 Ampeg_Release=2 foo=1\n");
        assert_eq!(i.regions().len(), 1);
        assert_eq!(i.regions()[0].props().ampeg_release, 2.0);
        let msgs: Vec<&str> = i.diagnostics().iter().map(|d| d.message.as_str()).collect();
        assert!(msgs[0].contains("not under any header"), "{msgs:?}");
        assert!(msgs[1].contains("unknown header <Region>"), "{msgs:?}");
        assert!(msgs[2].contains("unknown opcode `foo`"), "{msgs:?}");
    }

    #[test]
    fn control_settings() {
        let i = load(
            "<control> label_cc1=Dynamics set_cc1=64 set_hdcc11=1 label_key60=Hit octave_offset=1 note_offset=2\n\
             <region> sample=*sine sw_last=60 key=60 trigger=release\n",
        );
        let c = i.control();
        assert_eq!(c.label_cc[&1], "Dynamics");
        assert_eq!((c.set_cc[&1], c.set_cc[&11]), (64.0, 127.0));
        assert_eq!(c.label_key[&60], "Hit");
        let p = i.regions()[0].props();
        assert_eq!((p.lokey, p.sw_last), (74, Some((74, 74))));
        assert_eq!(p.trigger, Trigger::Release);
    }

    #[test]
    fn includes_are_relative_to_the_root_and_cycles_stop() {
        let dir = std::env::temp_dir().join(format!("flux-sfz-inc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inc/deeper")).unwrap();
        std::fs::write(dir.join("inc/a.sfz"), "<group> volume=-3\n#include \"inc/deeper/b.sfz\"\n").unwrap();
        std::fs::write(dir.join("inc/deeper/b.sfz"), "lokey=$LO\n<region> sample=*sine\n").unwrap();
        std::fs::write(dir.join("self.sfz"), "<region> sample=*saw\n#include \"self.sfz\"\n").unwrap();
        std::fs::write(dir.join("root.sfz"), "#define $LO 40\n#include \"inc/a.sfz\"\n#include \"missing.sfz\"\n").unwrap();
        let i = Instrument::load(dir.join("root.sfz")).unwrap();
        assert_eq!(i.files().len(), 3);
        assert_eq!(i.regions().len(), 1);
        let p = i.regions()[0].props();
        assert_eq!((p.volume, p.lokey), (-3.0, 40));
        assert!(i.errors().any(|d| d.message.contains("cannot include \"missing.sfz\"")));
        let s = Instrument::load(dir.join("self.sfz")).unwrap();
        assert_eq!(s.regions().len(), 1);
        assert!(s.errors().any(|d| d.message.contains("include cycle")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flat_text_round_trips_the_model() {
        let i = load("#define $V -3\n<group> volume=$V\n<region> sample=*sine key=60\n");
        let flat = i.to_flat_text();
        assert_eq!(flat, "<group>\nvolume=-3\n\n<region>\nsample=*sine\nkey=60\n\n");
        let j = load(&flat);
        assert_eq!(i.regions()[0].props(), j.regions()[0].props());
    }
}
