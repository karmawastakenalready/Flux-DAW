//! Building instrument sets, with a manifest so that only stale wrappers are made again.
//!
//! [`build`] generates the wrappers of a set (see [`crate::wrap`]) and writes `<output>/<set>.manifest.json`
//! next to them. For every instrument the manifest records a *recipe* hash (the instrument's definition, the
//! output settings and the generator's revision), the hash of every file read to make the wrapper (the source
//! SFZ and the files it includes), and the hash of every file written. That is enough to tell, without
//! generating anything:
//!
//! * which wrappers are up to date, so `build` skips them;
//! * which are stale (the set definition, a source file or the generator changed) or missing ([`check`]);
//! * which generated files were edited by hand since: `build` does not overwrite those without `force`, and does
//!   not delete them when they are no longer generated.
//!
//! Files are written atomically (a temporary file in the same folder, then a rename), the manifest last, so an
//! interrupted build leaves either the old or the new version of each file and a manifest that describes
//! what is on disk well enough for the next build to repair it.
//!
//! Sample files are not inputs: a wrapper only names them, so adding or replacing samples never makes it
//! stale (missing samples are reported by the build, and by [`crate::Instrument`] diagnostics).

use crate::error::{Diagnostic, Error, Result};
use crate::fsutil::{relative_path, slash_path};
use crate::set::{SetFile, Wrap};
use crate::wrap::{Generated, generate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// The manifest file format written by this version.
pub const MANIFEST_FORMAT: u32 = 1;

/// Bumped whenever the generated text changes for the same inputs, so that existing wrappers become stale.
pub const GENERATOR_REVISION: u32 = 1;

/// The generator named in manifests and recipes.
pub fn generator() -> String {
    format!("flux-sfz {} (wrappers r{GENERATOR_REVISION})", env!("CARGO_PKG_VERSION"))
}

/// What a build wrote for a set: `<output>/<set>.manifest.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub generator: String,
    pub set: String,
    /// The set definition (relative to the project root, `/` separators).
    pub set_file: String,
    /// In set order.
    pub instruments: Vec<ManifestEntry>,
}

/// One instrument in a [`Manifest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub name: String,
    pub wrap: Wrap,
    /// SHA-256 of the recipe (see [`recipe_hash`]).
    pub recipe: String,
    /// The SFZ to load (the wrapper, or the source for `wrap = "none"`).
    pub load: String,
    pub inputs: Vec<FileHash>,
    pub outputs: Vec<FileHash>,
}

/// A file and the SHA-256 of its bytes. Paths are relative to the project root with `/` separators (absolute
/// when the file is outside it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHash {
    pub path: String,
    pub sha256: String,
}

/// Where a set's manifest is.
pub fn manifest_path(set: &SetFile) -> PathBuf {
    set.output.join(format!("{}.manifest.json", set.name))
}

/// Read a set's manifest: `None` if the set was never built.
pub fn read_manifest(set: &SetFile) -> Result<Option<Manifest>> {
    let path = manifest_path(set);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&path, e)),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::Format {
        path: path.clone(),
        message: format!("not a flux-sfz manifest ({e}); delete it or build with force"),
    })?;
    if manifest.format != MANIFEST_FORMAT {
        return Err(Error::Format {
            path,
            message: format!("manifest format {} is not supported (this version reads {MANIFEST_FORMAT})", manifest.format),
        });
    }
    Ok(Some(manifest))
}

/// SHA-256 of bytes, as lower-case hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The recipe of an instrument: everything besides the input files that decides what is generated. Changing
/// the instrument's definition, the set's name, output folder or sample path style, or the generator, changes
/// it. With absolute sample paths the project root is part of it too (moving the project changes the paths
/// written into the wrappers).
pub fn recipe_hash(set: &SetFile, name: &str) -> Result<String> {
    let spec = set.instrument(name).ok_or_else(|| Error::Invalid(format!("set `{}` has no instrument `{name}`", set.name)))?;
    let mut recipe = serde_json::json!({
        "generator": generator(),
        "set": set.name,
        "instrument": name,
        "output": rel(&set.root, &set.output),
        "sample_paths": set.sample_paths,
        "spec": spec,
    });
    if set.sample_paths == crate::edit::SamplePaths::Absolute {
        recipe["root"] = serde_json::Value::String(slash_path(&set.root));
    }
    Ok(sha256_hex(recipe.to_string().as_bytes()))
}

/// A path for a manifest or report: relative to the root, `/` separators; absolute outside the root.
fn rel(root: &Path, path: &Path) -> String {
    match relative_path(root, path) {
        Some(r) if !r.starts_with("..") => r,
        _ => slash_path(path),
    }
}

fn unrel(root: &Path, path: &str) -> PathBuf {
    crate::fsutil::lexical_normalize(&root.join(path))
}

/// Why an instrument is not up to date.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "path", rename_all = "snake_case")]
pub enum Reason {
    /// Never built (no manifest, or the manifest has no entry for it).
    NotBuilt,
    /// The set definition of the instrument, the output settings or the generator changed.
    RecipeChanged,
    InputChanged(String),
    InputMissing(String),
    OutputMissing(String),
    /// Edited by hand after it was generated: `build` keeps it unless forced.
    OutputModified(String),
    /// Generated again for the check, the result differs from the file (only with [`CheckOptions::regenerate`]).
    OutputDiffers(String),
}

/// The state of one instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The wrapper matches its manifest entry and everything it was made from.
    UpToDate,
    /// `wrap = "none"`: played from the source, which exists.
    Direct,
    /// `build` would make it (again): see the reasons.
    Stale,
    /// It cannot be built or played: a source file is missing.
    Broken,
}

/// The result of [`check`] for one instrument.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstrumentCheck {
    pub name: String,
    pub wrap: Wrap,
    pub status: Status,
    /// The SFZ to load (relative to the root).
    pub load: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<Reason>,
}

/// The result of [`check`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetCheck {
    pub set: String,
    pub manifest: String,
    /// False when the set was never built.
    pub manifest_found: bool,
    pub instruments: Vec<InstrumentCheck>,
    /// Instruments in the manifest that the set no longer has; `build` deletes their files (unless edited).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

impl SetCheck {
    /// True when nothing needs building and every source is there.
    pub fn is_clean(&self) -> bool {
        self.removed.is_empty() && self.instruments.iter().all(|i| matches!(i.status, Status::UpToDate | Status::Direct))
    }
}

/// Options of [`check`].
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckOptions {
    /// Also generate every wrapper and compare it with the file on disk (slower; catches generator changes that
    /// did not bump [`GENERATOR_REVISION`]).
    pub regenerate: bool,
}

/// Hashes files, reading each once.
#[derive(Default)]
struct Hasher {
    known: HashMap<PathBuf, Option<String>>,
}

impl Hasher {
    /// `None` if the file does not exist; an error if it exists but cannot be read.
    fn hash(&mut self, path: &Path) -> Result<Option<String>> {
        if let Some(h) = self.known.get(path) {
            return Ok(h.clone());
        }
        let h = match std::fs::read(path) {
            Ok(b) => Some(sha256_hex(&b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::io(path, e)),
        };
        self.known.insert(path.to_path_buf(), h.clone());
        Ok(h)
    }
}

/// Compare one instrument with its manifest entry. A `wrap = "none"` instrument is `Direct` whatever the
/// reasons (they only tell whether its manifest entry is current).
fn instrument_state(
    set: &SetFile,
    name: &str,
    wrap: Wrap,
    entry: Option<&ManifestEntry>,
    hasher: &mut Hasher,
) -> Result<(Status, Vec<Reason>)> {
    let direct = wrap == Wrap::None;
    if direct {
        let spec = set.instrument(name).ok_or_else(|| Error::Invalid(format!("no instrument `{name}`")))?;
        let src = set.resolve(spec.source.as_deref().unwrap_or_default());
        if hasher.hash(&src)?.is_none() {
            return Ok((Status::Broken, vec![Reason::InputMissing(rel(&set.root, &src))]));
        }
    }
    let Some(entry) = entry else {
        return Ok((if direct { Status::Direct } else { Status::Stale }, vec![Reason::NotBuilt]));
    };
    let mut reasons = vec![];
    if entry.recipe != recipe_hash(set, name)? {
        reasons.push(Reason::RecipeChanged);
    }
    let mut broken = false;
    for input in &entry.inputs {
        match hasher.hash(&unrel(&set.root, &input.path))? {
            None => {
                reasons.push(Reason::InputMissing(input.path.clone()));
                broken = true;
            }
            Some(h) if h != input.sha256 => reasons.push(Reason::InputChanged(input.path.clone())),
            Some(_) => {}
        }
    }
    for output in &entry.outputs {
        match hasher.hash(&unrel(&set.root, &output.path))? {
            None => reasons.push(Reason::OutputMissing(output.path.clone())),
            Some(h) if h != output.sha256 => reasons.push(Reason::OutputModified(output.path.clone())),
            Some(_) => {}
        }
    }
    let status = if broken {
        Status::Broken
    } else if direct {
        Status::Direct
    } else if reasons.is_empty() {
        Status::UpToDate
    } else {
        Status::Stale
    };
    Ok((status, reasons))
}

/// Which instruments of a set are up to date, stale or broken, from the manifest and the files on disk
/// (nothing is written).
pub fn check(set: &SetFile, options: CheckOptions) -> Result<SetCheck> {
    let manifest = read_manifest(set)?;
    let mut hasher = Hasher::default();
    let mut instruments = vec![];
    for (name, spec) in &set.instruments {
        let entry = manifest.as_ref().and_then(|m| m.instruments.iter().find(|e| &e.name == name));
        let (mut status, mut reasons) = instrument_state(set, name, spec.wrap, entry, &mut hasher)?;
        if status == Status::Direct {
            reasons.clear();
        }
        let mut load = entry.map(|e| e.load.clone());
        if options.regenerate && spec.wrap != Wrap::None && status != Status::Broken {
            match generate(set, name) {
                Ok(g) => {
                    load = Some(rel(&set.root, &g.load));
                    for f in &g.files {
                        let on_disk = hasher.hash(&f.path)?;
                        if on_disk.is_some() && on_disk != Some(sha256_hex(&f.bytes())) {
                            reasons.push(Reason::OutputDiffers(rel(&set.root, &f.path)));
                            status = Status::Stale;
                        }
                    }
                }
                Err(_) => status = Status::Broken,
            }
        }
        let load = load.unwrap_or_else(|| rel(&set.root, &expected_load(set, name)));
        instruments.push(InstrumentCheck { name: name.clone(), wrap: spec.wrap, status, load, reasons });
    }
    let removed = manifest
        .as_ref()
        .map(|m| m.instruments.iter().filter(|e| set.instrument(&e.name).is_none()).map(|e| e.name.clone()).collect())
        .unwrap_or_default();
    Ok(SetCheck { set: set.name.clone(), manifest: rel(&set.root, &manifest_path(set)), manifest_found: manifest.is_some(), instruments, removed })
}

/// Where an instrument's SFZ to load will be, without generating it.
pub fn expected_load(set: &SetFile, name: &str) -> PathBuf {
    let Some(spec) = set.instrument(name) else { return set.output.join(format!("{name}.sfz")) };
    match spec.wrap {
        Wrap::None => set.resolve(spec.source.as_deref().unwrap_or_default()),
        Wrap::Patch => {
            let file = spec.source.as_deref().and_then(|s| Path::new(s).file_name()).map(Path::new).unwrap_or(Path::new("main.sfz"));
            set.output.join(name).join(file)
        }
        _ => set.output.join(format!("{name}.sfz")),
    }
}

/// Options of [`build`].
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    /// Generate every wrapper even when up to date, and overwrite generated files that were edited by hand.
    pub force: bool,
    /// Report what would be done without writing or deleting anything.
    pub dry_run: bool,
    /// Build only these instruments (all when empty). Files of instruments removed from the set are only
    /// deleted by a full build.
    pub only: Vec<String>,
}

/// What [`build`] did with one instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildAction {
    /// Generated and written (or, in a dry run, would be).
    Built,
    UpToDate,
    /// `wrap = "none"`: nothing to generate.
    Direct,
    /// Not built: see `error`. Its old files and manifest entry are left as they were.
    Failed,
    /// Not in [`BuildOptions::only`].
    Skipped,
}

/// The result of [`build`] for one instrument.
#[derive(Clone, Debug, Serialize)]
pub struct InstrumentBuild {
    pub name: String,
    pub wrap: Wrap,
    pub action: BuildAction,
    /// Why it was built (empty for up-to-date instruments and forced builds of up-to-date ones).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<Reason>,
    /// The SFZ to load (relative to the root).
    pub load: String,
    /// Files written (relative to the root).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub written: Vec<String>,
    /// Files that existed but were not made by a previous build (no manifest record) and were replaced.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replaced_unrecorded: Vec<String>,
    /// Problems in the sources (missing samples...) and warnings about the result.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The result of [`build`].
#[derive(Clone, Debug, Serialize)]
pub struct BuildReport {
    pub set: String,
    pub manifest: String,
    pub dry_run: bool,
    pub instruments: Vec<InstrumentBuild>,
    /// Files of earlier builds that are no longer generated, deleted (relative to the root).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<String>,
    /// Files no longer generated that were kept because they were edited by hand.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept_modified: Vec<String>,
}

impl BuildReport {
    pub fn failed(&self) -> impl Iterator<Item = &InstrumentBuild> {
        self.instruments.iter().filter(|i| i.action == BuildAction::Failed)
    }
}

/// [`build`] several sets. [`BuildOptions::only`] may name instruments of any of them: each set builds those it
/// has and sets with none of them are left out. A name that no set has is an error, before anything is built.
pub fn build_sets(sets: &[SetFile], options: &BuildOptions) -> Result<Vec<BuildReport>> {
    for name in &options.only {
        if !sets.iter().any(|s| s.instrument(name).is_some()) {
            return Err(Error::Invalid(format!("no instrument `{name}` in the sets given")));
        }
    }
    let mut reports = vec![];
    for set in sets {
        let only: Vec<String> = options.only.iter().filter(|n| set.instrument(n).is_some()).cloned().collect();
        if !options.only.is_empty() && only.is_empty() {
            continue;
        }
        reports.push(build(set, &BuildOptions { only, ..options.clone() })?);
    }
    Ok(reports)
}

/// Generate the stale wrappers of a set and update its manifest. See the [module documentation](self).
///
/// An instrument that fails (a missing source, an invalid patch...) is reported in the result and does not
/// stop the others; the call itself fails only when the manifest cannot be read or written.
pub fn build(set: &SetFile, options: &BuildOptions) -> Result<BuildReport> {
    for name in &options.only {
        if set.instrument(name).is_none() {
            return Err(Error::Invalid(format!("set `{}` has no instrument `{name}`", set.name)));
        }
    }
    let old = match read_manifest(set) {
        Ok(m) => m,
        // force rebuilds everything, so an unreadable manifest is replaced
        Err(_) if options.force => None,
        Err(e) => return Err(e),
    };
    let old_entry = |name: &str| old.as_ref().and_then(|m| m.instruments.iter().find(|e| e.name == name));
    let mut hasher = Hasher::default();
    let mut results = vec![];
    let mut entries: Vec<ManifestEntry> = vec![];
    // outputs recorded by the old manifest, with their recorded hashes
    let recorded: HashMap<PathBuf, String> = old
        .iter()
        .flat_map(|m| &m.instruments)
        .flat_map(|e| &e.outputs)
        .map(|o| (unrel(&set.root, &o.path), o.sha256.clone()))
        .collect();
    for (name, spec) in &set.instruments {
        let entry = old_entry(name);
        let selected = options.only.is_empty() || options.only.contains(name);
        if !selected {
            if let Some(e) = entry {
                entries.push(e.clone());
            }
            results.push(simple_result(set, name, spec.wrap, BuildAction::Skipped, entry));
            continue;
        }
        let (status, reasons) = instrument_state(set, name, spec.wrap, entry, &mut hasher)?;
        let fresh = status == Status::UpToDate || (status == Status::Direct && reasons.is_empty());
        if let (true, false, Some(e)) = (fresh, options.force, entry) {
            entries.push(e.clone());
            let action = if status == Status::Direct { BuildAction::Direct } else { BuildAction::UpToDate };
            results.push(simple_result(set, name, spec.wrap, action, entry));
            continue;
        }
        let modified: Vec<String> = reasons
            .iter()
            .filter_map(|r| match r {
                Reason::OutputModified(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        let mut result = InstrumentBuild {
            name: name.clone(),
            wrap: spec.wrap,
            action: BuildAction::Built,
            reasons,
            load: rel(&set.root, &expected_load(set, name)),
            written: vec![],
            replaced_unrecorded: vec![],
            diagnostics: vec![],
            error: None,
        };
        let generated = match generate(set, name) {
            Ok(g) => g,
            Err(e) => {
                result.action = BuildAction::Failed;
                result.error = Some(e.to_string());
                if let Some(e) = entry {
                    entries.push(e.clone());
                }
                results.push(result);
                continue;
            }
        };
        result.load = rel(&set.root, &generated.load);
        result.diagnostics = generated.diagnostics.clone();
        if spec.wrap == Wrap::None {
            result.action = BuildAction::Direct;
            result.reasons.clear();
            entries.push(new_entry(set, name, &generated, &mut hasher)?);
            results.push(result);
            continue;
        }
        if !modified.is_empty() && !options.force {
            result.action = BuildAction::Failed;
            result.error = Some(format!(
                "{} edited by hand since it was generated; move the change into the set definition, or build with \
                 force to overwrite it",
                if modified.len() == 1 { format!("{} was", modified[0]) } else { format!("{} were", modified.join(", ")) }
            ));
            if let Some(e) = entry {
                entries.push(e.clone());
            }
            results.push(result);
            continue;
        }
        let mut written: Vec<FileHash> = vec![];
        let mut write_error = None;
        for f in &generated.files {
            let bytes = f.bytes();
            if hasher.hash(&f.path)?.is_some() && !recorded.contains_key(&f.path) {
                result.replaced_unrecorded.push(rel(&set.root, &f.path));
            }
            if !options.dry_run
                && let Err(e) = write_atomic(&f.path, &bytes)
            {
                write_error = Some(e.to_string());
                break;
            }
            let sha = sha256_hex(&bytes);
            hasher.known.insert(f.path.clone(), Some(sha.clone()));
            written.push(FileHash { path: rel(&set.root, &f.path), sha256: sha });
        }
        result.written = written.iter().map(|f| f.path.clone()).collect();
        if let Some(e) = write_error {
            result.action = BuildAction::Failed;
            result.error = Some(e);
            // record what this build wrote (and what the old build wrote that is still there), with an empty
            // recipe so that the next build makes the instrument again
            let mut partial = entry.cloned().unwrap_or_else(|| ManifestEntry {
                name: name.clone(),
                wrap: spec.wrap,
                recipe: String::new(),
                load: result.load.clone(),
                inputs: vec![],
                outputs: vec![],
            });
            partial.recipe = String::new();
            partial.outputs.retain(|o| !written.iter().any(|w| w.path == o.path));
            partial.outputs.extend(written);
            entries.push(partial);
            results.push(result);
            continue;
        }
        entries.push(new_entry(set, name, &generated, &mut hasher)?);
        results.push(result);
    }
    // outputs of the old build that nothing generates now
    if !options.only.is_empty()
        && let Some(m) = &old
    {
        // a partial build keeps the entries of instruments removed from the set
        entries.extend(m.instruments.iter().filter(|e| set.instrument(&e.name).is_none()).cloned());
    }
    let current: BTreeSet<PathBuf> =
        entries.iter().flat_map(|e| &e.outputs).map(|o| unrel(&set.root, &o.path)).collect();
    let mut deleted = vec![];
    let mut kept_modified = vec![];
    let mut stale: Vec<(&PathBuf, &String)> = recorded.iter().filter(|(p, _)| !current.contains(*p)).collect();
    stale.sort();
    for (path, sha) in stale {
        match hasher.hash(path)? {
            None => {}
            Some(h) if &h == sha || options.force => {
                if !options.dry_run {
                    std::fs::remove_file(path).map_err(|e| Error::io(path, e))?;
                    remove_empty_dirs(path, &set.output);
                }
                deleted.push(rel(&set.root, path));
            }
            Some(_) => kept_modified.push(rel(&set.root, path)),
        }
    }
    let manifest = Manifest {
        format: MANIFEST_FORMAT,
        generator: generator(),
        set: set.name.clone(),
        set_file: rel(&set.root, &set.path),
        instruments: entries,
    };
    let path = manifest_path(set);
    if !options.dry_run && old.as_ref() != Some(&manifest) {
        let mut json = serde_json::to_string_pretty(&manifest).map_err(|e| Error::Format { path: path.clone(), message: e.to_string() })?;
        json.push('\n');
        write_atomic(&path, json.as_bytes())?;
    }
    Ok(BuildReport { set: set.name.clone(), manifest: rel(&set.root, &path), dry_run: options.dry_run, instruments: results, deleted, kept_modified })
}

fn simple_result(set: &SetFile, name: &str, wrap: Wrap, action: BuildAction, entry: Option<&ManifestEntry>) -> InstrumentBuild {
    InstrumentBuild {
        name: name.to_owned(),
        wrap,
        action,
        reasons: vec![],
        load: entry.map_or_else(|| rel(&set.root, &expected_load(set, name)), |e| e.load.clone()),
        written: vec![],
        replaced_unrecorded: vec![],
        diagnostics: vec![],
        error: None,
    }
}

fn new_entry(set: &SetFile, name: &str, g: &Generated, hasher: &mut Hasher) -> Result<ManifestEntry> {
    let mut inputs = vec![];
    for p in &g.inputs {
        if let Some(h) = hasher.hash(p)? {
            inputs.push(FileHash { path: rel(&set.root, p), sha256: h });
        }
    }
    let outputs =
        g.files.iter().map(|f| FileHash { path: rel(&set.root, &f.path), sha256: sha256_hex(&f.bytes()) }).collect();
    Ok(ManifestEntry {
        name: name.to_owned(),
        wrap: g.wrap,
        recipe: recipe_hash(set, name)?,
        load: rel(&set.root, &g.load),
        inputs,
        outputs,
    })
}

/// Write a file through a temporary file in the same folder and a rename, so readers never see half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(path, e)
    })
}

/// Remove the folders left empty by deleting `file`, up to (not including) `stop`.
fn remove_empty_dirs(file: &Path, stop: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == stop || !d.starts_with(stop) || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// A file or folder in a set's output folder that no set accounts for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Untracked {
    /// The set whose output folder holds it (the innermost one when output folders are nested).
    pub set: String,
    /// Relative to the root; folders end with `/`.
    pub path: String,
}

/// Files and folders in the output folders of `sets` that none of them generates (left over from removed
/// instruments, older tools, or made by hand). A folder none of whose contents is accounted for is listed once.
/// Files that may be generated are accounted for even before the first build.
pub fn untracked(sets: &[SetFile]) -> Result<Vec<Untracked>> {
    let mut owned_files: BTreeSet<PathBuf> = BTreeSet::new();
    let mut owned_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut outputs: BTreeMap<PathBuf, String> = BTreeMap::new();
    for set in sets {
        outputs.insert(set.output.clone(), set.name.clone());
        owned_files.insert(manifest_path(set));
        for (name, spec) in &set.instruments {
            match spec.wrap {
                Wrap::None => {}
                Wrap::Patch => {
                    owned_dirs.insert(set.output.join(name));
                }
                _ => {
                    owned_files.insert(set.output.join(format!("{name}.sfz")));
                }
            }
        }
        if let Ok(Some(m)) = read_manifest(set) {
            owned_files.extend(m.instruments.iter().flat_map(|e| &e.outputs).map(|o| unrel(&set.root, &o.path)));
        }
    }
    let root = sets.first().map(|s| s.root.clone()).unwrap_or_default();
    let mut found = vec![];
    for (dir, set) in &outputs {
        walk_untracked(dir, dir, set, &outputs, &owned_files, &owned_dirs, &root, &mut found)?;
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

#[allow(clippy::too_many_arguments)]
fn walk_untracked(
    dir: &Path,
    top: &Path,
    set: &str,
    outputs: &BTreeMap<PathBuf, String>,
    owned_files: &BTreeSet<PathBuf>,
    owned_dirs: &BTreeSet<PathBuf>,
    root: &Path,
    found: &mut Vec<Untracked>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(dir, e)),
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            if owned_dirs.contains(&p) || (p.as_path() != top && outputs.contains_key(&p)) {
                continue;
            }
            let holds_owned = owned_files.iter().chain(owned_dirs).chain(outputs.keys()).any(|o| o.starts_with(&p));
            if holds_owned {
                walk_untracked(&p, top, set, outputs, owned_files, owned_dirs, root, found)?;
            } else {
                found.push(Untracked { set: set.to_owned(), path: format!("{}/", rel(root, &p)) });
            }
        } else if !owned_files.contains(&p) {
            found.push(Untracked { set: set.to_owned(), path: rel(root, &p) });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flux-sfz-manifest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A project with a tiny library and a set of three instruments.
    fn project(name: &str) -> (PathBuf, SetFile) {
        let d = tmp(name);
        std::fs::create_dir_all(d.join("instruments/lib/samples")).unwrap();
        std::fs::write(d.join("instruments/lib/samples/a_v1.wav"), b"x").unwrap();
        std::fs::write(d.join("instruments/lib/samples/a_v3.wav"), b"x").unwrap();
        std::fs::write(
            d.join("instruments/lib/harp.sfz"),
            "<control> default_path=samples/\n<global> ampeg_release=1\n<region> sample=a_v1.wav key=60\n",
        )
        .unwrap();
        std::fs::write(
            d.join("instruments/lib/strings.sfz"),
            "<control> default_path=samples/\n<group> hivel=64\n<region> sample=a_v1.wav key=60\n<group> lovel=65\n<region> sample=a_v3.wav key=60\n",
        )
        .unwrap();
        std::fs::create_dir_all(d.join("sets")).unwrap();
        std::fs::write(
            d.join("sets/song.toml"),
            "[instruments.harp]\nsource = \"instruments/lib/harp.sfz\"\nwrap = \"expr\"\nextra_global = { ampeg_release = \"3\" }\n\n\
             [instruments.strings]\nwrap = \"keyswitch\"\narticulations = [{ name = \"sus\", source = \"instruments/lib/strings.sfz\", crossfade = true }, { name = \"pluck\", source = \"instruments/lib/harp.sfz\" }]\n\n\
             [instruments.raw]\nsource = \"instruments/lib/harp.sfz\"\nwrap = \"none\"\n",
        )
        .unwrap();
        let set = SetFile::load(&d.join("sets/song.toml"), &d).unwrap();
        (d, set)
    }

    fn actions(r: &BuildReport) -> Vec<(String, BuildAction)> {
        r.instruments.iter().map(|i| (i.name.clone(), i.action)).collect()
    }

    fn statuses(c: &SetCheck) -> Vec<Status> {
        c.instruments.iter().map(|i| i.status).collect()
    }

    #[test]
    fn builds_once_then_skips() {
        let (d, set) = project("once");
        let c = check(&set, CheckOptions::default()).unwrap();
        assert!(!c.manifest_found);
        assert_eq!(statuses(&c), [Status::Stale, Status::Stale, Status::Direct]);
        assert_eq!(c.instruments[0].reasons, [Reason::NotBuilt]);

        let dry = build(&set, &BuildOptions { dry_run: true, ..BuildOptions::default() }).unwrap();
        assert_eq!(dry.instruments[0].action, BuildAction::Built);
        assert!(!manifest_path(&set).exists() && !set.output.join("harp.sfz").exists());

        let r = build(&set, &BuildOptions::default()).unwrap();
        assert_eq!(
            actions(&r),
            [("harp".into(), BuildAction::Built), ("strings".into(), BuildAction::Built), ("raw".into(), BuildAction::Direct)]
        );
        assert_eq!(r.instruments[0].written, ["build/sfz/song/harp.sfz"]);
        assert_eq!(r.instruments[2].load, "instruments/lib/harp.sfz");
        let text = std::fs::read_to_string(set.output.join("harp.sfz")).unwrap();
        assert!(text.contains("default_path=../../../instruments/lib/samples/"), "{text}");
        let m = read_manifest(&set).unwrap().unwrap();
        assert_eq!(m.set_file, "sets/song.toml");
        assert_eq!(m.instruments[1].inputs.len(), 2);

        assert!(check(&set, CheckOptions { regenerate: true }).unwrap().is_clean());
        let again = build(&set, &BuildOptions::default()).unwrap();
        assert_eq!(again.instruments[0].action, BuildAction::UpToDate);
        assert_eq!(again.instruments[1].action, BuildAction::UpToDate);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn finds_stale_and_edited_wrappers() {
        let (d, set) = project("stale");
        build(&set, &BuildOptions::default()).unwrap();

        // a source changes: both wrappers made from it are stale
        let harp = d.join("instruments/lib/harp.sfz");
        std::fs::write(&harp, "<control> default_path=samples/\n<global> ampeg_release=2\n<region> sample=a_v1.wav key=61\n").unwrap();
        let c = check(&set, CheckOptions::default()).unwrap();
        assert_eq!(statuses(&c), [Status::Stale, Status::Stale, Status::Direct]);
        assert_eq!(c.instruments[0].reasons, [Reason::InputChanged("instruments/lib/harp.sfz".into())]);

        // only harp is rebuilt
        let r = build(&set, &BuildOptions { only: vec!["harp".into()], ..BuildOptions::default() }).unwrap();
        assert_eq!(actions(&r)[0].1, BuildAction::Built);
        assert_eq!(actions(&r)[1].1, BuildAction::Skipped);
        assert!(std::fs::read_to_string(set.output.join("harp.sfz")).unwrap().contains("key=61"));
        assert_eq!(statuses(&check(&set, CheckOptions::default()).unwrap())[..2], [Status::UpToDate, Status::Stale]);

        // a wrapper edited by hand is not overwritten without force
        let wrapper = set.output.join("strings.sfz");
        std::fs::write(&wrapper, "// mine\n").unwrap();
        let r = build(&set, &BuildOptions::default()).unwrap();
        assert_eq!(r.instruments[1].action, BuildAction::Failed);
        assert!(r.instruments[1].error.as_deref().unwrap().contains("edited by hand"));
        assert_eq!(std::fs::read_to_string(&wrapper).unwrap(), "// mine\n");
        let r = build(&set, &BuildOptions { force: true, ..BuildOptions::default() }).unwrap();
        assert_eq!(r.instruments[1].action, BuildAction::Built);
        assert!(check(&set, CheckOptions::default()).unwrap().is_clean());

        // a missing output is reported and rebuilt
        std::fs::remove_file(set.output.join("harp.sfz")).unwrap();
        let c = check(&set, CheckOptions::default()).unwrap();
        assert_eq!(c.instruments[0].reasons, [Reason::OutputMissing("build/sfz/song/harp.sfz".into())]);
        build(&set, &BuildOptions::default()).unwrap();
        assert!(set.output.join("harp.sfz").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn recipe_changes_and_removed_instruments() {
        let (d, set) = project("recipe");
        build(&set, &BuildOptions::default()).unwrap();
        std::fs::write(set.output.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(set.output.join("old/deep")).unwrap();
        std::fs::write(set.output.join("old/deep/a.sfz"), "x").unwrap();

        // harp changes its extra_global, strings is removed
        std::fs::write(
            d.join("sets/song.toml"),
            "[instruments.harp]\nsource = \"instruments/lib/harp.sfz\"\nwrap = \"expr\"\nextra_global = { ampeg_release = \"4\" }\n\n\
             [instruments.raw]\nsource = \"instruments/lib/harp.sfz\"\nwrap = \"none\"\n",
        )
        .unwrap();
        let set = SetFile::load(&d.join("sets/song.toml"), &d).unwrap();
        let c = check(&set, CheckOptions::default()).unwrap();
        assert_eq!(c.instruments[0].reasons, [Reason::RecipeChanged]);
        assert_eq!(c.removed, ["strings"]);
        let u = untracked(std::slice::from_ref(&set)).unwrap();
        let paths: Vec<&str> = u.iter().map(|u| u.path.as_str()).collect();
        // strings.sfz is still in the manifest: the next build deletes it
        assert_eq!(paths, ["build/sfz/song/notes.txt", "build/sfz/song/old/"]);

        let r = build(&set, &BuildOptions::default()).unwrap();
        assert_eq!(r.deleted, ["build/sfz/song/strings.sfz"]);
        assert!(!set.output.join("strings.sfz").exists());
        assert!(std::fs::read_to_string(set.output.join("harp.sfz")).unwrap().contains("ampeg_release=4"));
        assert!(check(&set, CheckOptions::default()).unwrap().is_clean());

        // a missing source makes the instrument broken
        std::fs::remove_file(d.join("instruments/lib/harp.sfz")).unwrap();
        let c = check(&set, CheckOptions::default()).unwrap();
        assert_eq!(statuses(&c), [Status::Broken, Status::Broken]);
        let r = build(&set, &BuildOptions::default()).unwrap();
        assert_eq!(r.failed().count(), 2);
        assert!(set.output.join("harp.sfz").exists(), "a failed build keeps the old wrapper");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn bad_manifests_are_errors() {
        let (d, set) = project("bad");
        std::fs::create_dir_all(&set.output).unwrap();
        std::fs::write(manifest_path(&set), "{").unwrap();
        assert!(check(&set, CheckOptions::default()).unwrap_err().to_string().contains("not a flux-sfz manifest"));
        assert!(build(&set, &BuildOptions::default()).is_err());
        assert!(build(&set, &BuildOptions { force: true, ..BuildOptions::default() }).is_ok());
        assert!(read_manifest(&set).unwrap().is_some());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
