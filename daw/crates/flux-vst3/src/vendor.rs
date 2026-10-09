//! Presets saved in plugins' own formats (`.vital`, Valhalla `.vpreset`, `.dspreset`, STL Tones banks): found in the
//! vendors' standard folders and turned into the component state that loads them, so they work headless.
//!
//! Component states ([`to_state`] builds them; the template, the plugin's current state, supplies ids and headers):
//! * JUCE VST2-compatible (Vital, Valhalla), big-endian:
//!   ```text
//!   "VstW"  u32 header size (8)  u32 version (1)  u32 bypass
//!   "CcnK"  u32 size of the rest  "FBCh"  u32 version  fxID  u32 fxVersion  u32 programs  128 reserved  u32 n  chunk[n]
//!   ```
//! * JUCE XML (`copyXmlToBinary`): `"VC2!"`, u32 LE text length, UTF-8 XML, NUL.
//! * JUCE private data, after a JUCE plugin's own data (the host's bypass flag): 8 zero bytes, a ValueTree, i64 LE its
//!   size, `"JUCEPrivateData"`. Kept from the template.
//!
//! Per family:
//! * Vital: chunk = the preset JSON (a `.vital` file as is, plus `preset_name`) and a NUL.
//! * Valhalla: chunk = JUCE XML `<ValhallaSupermassive presetName=.. Mix=.. ../>`; a `.vpreset` is that XML. Attributes
//!   the preset lacks (UI size, mix lock, parameters newer than the preset) keep the template's values.
//! * Decent Sampler: JUCE XML (no VST2 wrapper) of the `.dspreset` with the template's `_presetName`, `_samplePath`...
//!   on the root; `_samplePath` is the preset's folder, which sample paths are relative to. It loads the samples on a
//!   timer after `setState`, so a host must pump messages before they sound. Packed libraries (`.dslibrary`, zips)
//!   are not read.
//! * STL Tones (Emissary, NadIR): `'0'` and a JSON bank (a bank `.json` as is); `"Default Program"` is the current
//!   program. Their parser is picky (a number right before `}` breaks it), so banks are edited as text, never
//!   re-serialized.
//!
//! Preset files are untrusted: sizes are capped, folders are walked without following links and to a limited depth,
//! and malformed files are errors (or skipped when listing), never panics.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Largest preset file read.
const MAX_FILE: u64 = 64 << 20;
/// Largest bank read while listing (banks are a few KB; listing reads every one).
const MAX_LIST_FILE: u64 = 4 << 20;
/// How deep to look inside preset folders.
const MAX_DEPTH: usize = 8;
/// Directory entries looked at per listing (sample libraries hold many files).
const MAX_ENTRIES: usize = 200_000;

const JUCE_PRIVATE: &[u8] = b"JUCEPrivateData";
const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

/// Which preset format a plugin uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// Vital: `.vital` JSON in `Documents/Vital/<pack>/Presets`.
    Vital,
    /// Valhalla DSP plugins: `.vpreset` XML in `%PROGRAMDATA%/Valhalla DSP, LLC/<plugin>/Presets`.
    Valhalla,
    /// Decent Sampler: `.dspreset` XML (samples beside it) in `%APPDATA%/Decidedly/DecentSampler/Sample Libraries`.
    DecentSampler,
    /// STL Tones Ignite plugins (Emissary, NadIR): JSON banks of programs in `Documents/STL/**/Banks`.
    StlTones,
}

impl Family {
    /// Whether restoring a state can finish in the background: Decent Sampler reads the preset at once but loads
    /// its samples on its message thread, and drops every note until they're in (see [`crate::settle`]).
    pub fn loads_in_background(self) -> bool {
        matches!(self, Family::DecentSampler)
    }

    /// Preset file extension (lowercase, no dot).
    pub fn extension(self) -> &'static str {
        match self {
            Family::Vital => "vital",
            Family::Valhalla => "vpreset",
            Family::DecentSampler => "dspreset",
            Family::StlTones => "json",
        }
    }
}

/// The preset format of a plugin class, from its name and vendor.
pub fn family(class_name: &str, vendor: &str) -> Option<Family> {
    let (n, v) = (class_name.trim().to_lowercase(), vendor.trim().to_lowercase());
    if v.starts_with("vital audio") || (v.is_empty() && n == "vital") {
        Some(Family::Vital)
    } else if v.starts_with("valhalla dsp") {
        Some(Family::Valhalla)
    } else if v == "decidedly" || n == "decentsampler" {
        Some(Family::DecentSampler)
    } else if v == "stl tones" {
        Some(Family::StlTones)
    } else {
        None
    }
}

/// A preset in a plugin's own format.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VendorPreset {
    /// The file's name, or the program's name in a bank.
    pub name: String,
    /// Pack or folder, `/`-separated (`Afro`, `Factory/ECHOES/Dotted`); the bank for STL Tones.
    pub group: String,
    /// The preset file (the bank for STL Tones).
    pub path: PathBuf,
    /// Program index inside a bank file (STL Tones; None = the bank's own current program).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<usize>,
}

// ---- finding presets ---------------------------------------------------------------------------

/// Where the user's Documents folder may be (Windows: `%USERPROFILE%\Documents`, or OneDrive's when redirected).
fn documents() -> Vec<PathBuf> {
    let mut v = vec![];
    for var in ["USERPROFILE", "OneDrive", "HOME"] {
        if let Some(h) = std::env::var_os(var) {
            v.push(PathBuf::from(h).join("Documents"));
        }
    }
    v
}

/// `name` is usable as one path component (plugin names become folder names).
fn plain_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '\0'))
}

/// The standard folders holding `family`'s presets for `plugin_name` (existing ones, without duplicates).
pub fn dirs(family: Family, plugin_name: &str) -> Vec<PathBuf> {
    let mut v = vec![];
    match family {
        Family::Vital => v.extend(documents().into_iter().map(|d| d.join("Vital"))),
        Family::StlTones => v.extend(documents().into_iter().map(|d| d.join("STL"))),
        Family::Valhalla => {
            let name = plugin_name.trim();
            if plain_component(name) {
                for var in ["PROGRAMDATA", "ALLUSERSPROFILE"] {
                    if let Some(p) = std::env::var_os(var) {
                        v.push(PathBuf::from(p).join("Valhalla DSP, LLC").join(name).join("Presets"));
                    }
                }
            }
        }
        Family::DecentSampler => {
            if let Some(a) = std::env::var_os("APPDATA") {
                v.push(PathBuf::from(a).join("Decidedly").join("DecentSampler").join("Sample Libraries"));
            }
        }
    }
    v.retain(|d| d.is_dir());
    let mut seen = std::collections::HashSet::new();
    v.retain(|d| seen.insert(std::fs::canonicalize(d).unwrap_or_else(|_| d.clone())));
    v
}

/// Presets for `plugin_name` (the class name) in the standard folders ([`dirs`]). Sorted by group, then file, then
/// program.
pub fn list(family: Family, plugin_name: &str) -> Vec<VendorPreset> {
    list_in(family, plugin_name, &dirs(family, plugin_name))
}

/// Presets under `roots` (searched to a limited depth, links not followed; a file under two roots is listed once).
/// STL Tones banks are kept when the application that wrote them is `plugin_name`; unreadable or malformed ones are
/// skipped.
pub fn list_in(family: Family, plugin_name: &str, roots: &[PathBuf]) -> Vec<VendorPreset> {
    let mut files = vec![];
    let mut budget = MAX_ENTRIES;
    for root in roots {
        walk(root, root, 0, family.extension(), &mut budget, &mut files);
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|(p, _)| seen.insert(p.clone()));
    let mut out = vec![];
    for (path, rel) in files {
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        match family {
            Family::Vital => {
                let group = rel.iter().filter(|c| !c.eq_ignore_ascii_case("presets")).cloned().collect::<Vec<_>>().join("/");
                out.push(VendorPreset { name, group, path, program: None });
            }
            Family::Valhalla | Family::DecentSampler => out.push(VendorPreset { name, group: rel.join("/"), path, program: None }),
            Family::StlTones => {
                if !rel.last().is_some_and(|d| d.eq_ignore_ascii_case("banks")) {
                    continue;
                }
                let Ok(bytes) = read_limited(&path, MAX_LIST_FILE) else { continue };
                let Ok(bank) = stl_bank(&bytes) else { continue };
                if !stl_app_matches(plugin_name, bank.application.as_ref().map_or("", |a| a.name.as_str())) {
                    continue;
                }
                for (i, p) in bank.programs.iter().enumerate() {
                    out.push(VendorPreset { name: p.name.clone(), group: name.clone(), path: path.clone(), program: Some(i) });
                }
            }
        }
    }
    let key = |p: &VendorPreset| (p.group.to_lowercase(), p.path.to_string_lossy().to_lowercase(), p.program, p.name.clone());
    out.sort_by_cached_key(key);
    out
}

/// Collect files with extension `ext` under `dir`, with their folder path relative to `root`.
fn walk(root: &Path, dir: &Path, depth: usize, ext: &str, budget: &mut usize, out: &mut Vec<(PathBuf, Vec<String>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        // the entry's own type: links (and Windows junctions) are neither files nor folders here
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            if depth < MAX_DEPTH {
                walk(root, &p, depth + 1, ext, budget, out);
            }
        } else if ft.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) && e.metadata().is_ok_and(|m| m.len() <= MAX_FILE) {
            let rel = dir.strip_prefix(root).map(|r| r.iter().map(|c| c.to_string_lossy().into_owned()).collect()).unwrap_or_default();
            out.push((p, rel));
        }
    }
}

/// Read at most `limit` bytes of a file; larger files are an error.
fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let meta = f.metadata().with_context(|| format!("reading {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a file", path.display());
    }
    let mut b = Vec::with_capacity(meta.len().min(limit) as usize);
    f.take(limit + 1).read_to_end(&mut b).with_context(|| format!("reading {}", path.display()))?;
    if b.len() as u64 > limit {
        bail!("{} is larger than {} MB", path.display(), limit >> 20);
    }
    Ok(b)
}

// ---- building states ---------------------------------------------------------------------------

/// The component state that loads `preset` into a plugin of `family`. `template` is the plugin's current component
/// state (it supplies wrapper headers, ids and JUCE's private data).
pub fn to_state(family: Family, template: &[u8], preset: &VendorPreset) -> Result<Vec<u8>> {
    let path = &preset.path;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if family == Family::DecentSampler && matches!(ext.as_str(), "dslibrary" | "dsbundle" | "dsproduct" | "zip") {
        bail!("{} is a .{ext} library, not a preset: choose a .dspreset inside it (install or extract a packed one first)", path.display());
    }
    let data = read_limited(path, MAX_FILE)?;
    from_bytes(family, template, &data, preset).with_context(|| format!("loading {}", path.display()))
}

/// [`to_state`] for preset bytes already read (`preset.path` still names the file: Decent Sampler resolves samples
/// against it).
pub fn from_bytes(family: Family, template: &[u8], data: &[u8], preset: &VendorPreset) -> Result<Vec<u8>> {
    let name = if preset.name.is_empty() { preset.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default() } else { preset.name.clone() };
    match family {
        Family::Vital => vital_state(template, data, &name),
        Family::Valhalla => valhalla_state(template, data, &name),
        Family::DecentSampler => decent_state(template, data, &name, &preset.path),
        Family::StlTones => stl_state(template, data, preset.program),
    }
}

/// UTF-8 text of a preset file, without a BOM or trailing NULs.
fn text(data: &[u8]) -> Result<&str> {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    std::str::from_utf8(&data[..end]).map_err(|e| anyhow!("not UTF-8 text ({e})"))
}

fn vital_state(template: &[u8], data: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut w = Vst2::parse(template).context("the template is not a Vital state")?;
    let (own, private) = split_private(&w.chunk);
    if own.first() != Some(&b'{') {
        bail!("the template is not a Vital state (its chunk is not JSON)");
    }
    let t = text(data)?.trim();
    if !t.starts_with('{') {
        bail!("not a Vital preset (not a JSON object)");
    }
    #[derive(Deserialize)]
    struct Head {
        settings: Option<IgnoredAny>,
        preset_name: Option<IgnoredAny>,
    }
    let head: Head = serde_json::from_str(t).map_err(|e| anyhow!("not a Vital preset ({e})"))?;
    if head.settings.is_none() {
        bail!("not a Vital preset (no \"settings\")");
    }
    let mut chunk = Vec::with_capacity(t.len() + name.len() + 32 + private.len());
    if head.preset_name.is_some() {
        chunk.extend_from_slice(t.as_bytes());
    } else {
        // Vital names a preset loaded from a file after the file
        chunk.extend_from_slice(b"{\"preset_name\":");
        chunk.extend_from_slice(serde_json::to_string(name)?.as_bytes());
        chunk.push(b',');
        chunk.extend_from_slice(&t.as_bytes()[1..]);
    }
    chunk.push(0);
    chunk.extend_from_slice(private);
    w.chunk = chunk;
    w.to_bytes()
}

fn valhalla_state(template: &[u8], data: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut w = Vst2::parse(template).context("the template is not a Valhalla state")?;
    let (own, private) = split_private(&w.chunk);
    let txml = xml_from_binary(own).context("the template is not a Valhalla state")?;
    let ttag = root_tag(txml).context("the template's XML")?;
    let t = text(data)?;
    let ptag = root_tag(t).map_err(|e| anyhow!("not a Valhalla preset ({e})"))?;
    if ptag.name != ttag.name {
        bail!("the preset is for {}, not {}", ptag.name, ttag.name);
    }
    let mut tag = format!("<{}", ptag.name);
    for a in &ptag.attrs {
        a.write(&mut tag);
    }
    if !ptag.has("presetName") {
        Attr::write_new("presetName", name, &mut tag);
    }
    for a in ttag.attrs.iter().filter(|a| !ptag.has(a.name) && a.name != "presetName") {
        a.write(&mut tag);
    }
    let xml = format!("{XML_DECL} {tag}{}", after_tag(t, &ptag));
    let mut chunk = xml_to_binary(&xml)?;
    chunk.extend_from_slice(private);
    w.chunk = chunk;
    w.to_bytes()
}

fn decent_state(template: &[u8], data: &[u8], name: &str, path: &Path) -> Result<Vec<u8>> {
    let (own, private) = split_private(template);
    let txml = xml_from_binary(own).context("the template is not a Decent Sampler state")?;
    let ttag = root_tag(txml).context("the template's XML")?;
    if ttag.name != "DecentSampler" {
        bail!("the template is not a Decent Sampler state (root <{}>)", ttag.name);
    }
    let t = text(data)?;
    let ptag = root_tag(t).map_err(|e| anyhow!("not a .dspreset ({e})"))?;
    if ptag.name != "DecentSampler" {
        bail!("not a .dspreset (root <{}>, not <DecentSampler>)", ptag.name);
    }
    // sample paths are relative to this folder (a file here is taken as a folder too)
    let file = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let sample_path = file.parent().unwrap_or(&file).to_string_lossy();
    let mut tag = String::from("<DecentSampler");
    // the preset's own attributes (minVersion...); the state's `_` attributes come from the template
    for a in ptag.attrs.iter().filter(|a| !a.name.starts_with('_')) {
        a.write(&mut tag);
    }
    for a in ttag.attrs.iter().filter(|a| a.name.starts_with('_')) {
        match a.name {
            "_presetName" => Attr::write_new(a.name, name, &mut tag),
            "_samplePath" => Attr::write_new(a.name, &sample_path, &mut tag),
            "_libraryUrl" | "_libraryCanonicalUrl" | "_libraryBookmark" | "_dsProductId" => Attr::write_new(a.name, "", &mut tag),
            _ => a.write(&mut tag),
        }
    }
    if !ttag.has("_presetName") {
        Attr::write_new("_presetName", name, &mut tag);
    }
    if !ttag.has("_samplePath") {
        Attr::write_new("_samplePath", &sample_path, &mut tag);
    }
    let xml = format!("{XML_DECL} {tag}{}", after_tag(t, &ptag));
    let mut out = xml_to_binary(&xml)?;
    out.extend_from_slice(private);
    Ok(out)
}

/// The parts of an STL Tones bank that matter here.
#[derive(Deserialize)]
struct StlBank {
    #[serde(rename = "Application")]
    application: Option<StlApp>,
    #[serde(rename = "Programs")]
    programs: Vec<StlProgram>,
}

#[derive(Deserialize)]
struct StlApp {
    #[serde(rename = "Name", default)]
    name: String,
}

#[derive(Deserialize)]
struct StlProgram {
    #[serde(rename = "Name", default)]
    name: String,
}

fn stl_bank(data: &[u8]) -> Result<StlBank> {
    let t = text(data)?.trim();
    if !t.starts_with('{') {
        bail!("not an STL Tones bank (not a JSON object)");
    }
    serde_json::from_str(t).map_err(|e| anyhow!("not an STL Tones bank ({e})"))
}

/// A bank made by `app` ("NadIR") belongs to the plugin class `plugin` ("Ignite - NadIR"); an empty name takes any.
fn stl_app_matches(plugin: &str, app: &str) -> bool {
    let (p, a) = (plugin.trim().to_lowercase(), app.trim().to_lowercase());
    if p.is_empty() {
        return true;
    }
    !a.is_empty() && (p == a || p.strip_suffix(&a).is_some_and(|rest| rest.ends_with(|c: char| !c.is_alphanumeric())))
}

fn stl_state(template: &[u8], data: &[u8], program: Option<usize>) -> Result<Vec<u8>> {
    let start = template.iter().position(|&b| b == b'{').unwrap_or(template.len());
    let prefix = &template[..start];
    if prefix.is_empty() || prefix.len() > 8 || !prefix.iter().all(u8::is_ascii_graphic) {
        bail!("the template is not an STL Tones state");
    }
    let tbank = stl_bank(&template[start..]).context("the template is not an STL Tones state")?;
    let t = text(data)?.trim();
    let bank = stl_bank(t.as_bytes())?;
    let (tapp, app) = (tbank.application.map(|a| a.name).unwrap_or_default(), bank.application.map(|a| a.name).unwrap_or_default());
    if !tapp.is_empty() && !app.is_empty() && !tapp.eq_ignore_ascii_case(&app) {
        bail!("the bank is for {app}, not {tapp}");
    }
    let body = match program {
        None => t.to_string(),
        Some(p) => {
            if p >= bank.programs.len() {
                bail!("the bank has {} programs; there is no program {p}", bank.programs.len());
            }
            let (value, close) = json_top_level(t, "Default Program").ok_or_else(|| anyhow!("not an STL Tones bank (not one JSON object)"))?;
            // a space after the number: their parser fails on a number right before `}`
            match value {
                Some(r) => {
                    let gap = if t[r.end..].starts_with(char::is_whitespace) { "" } else { " " };
                    format!("{}{p}{gap}{}", &t[..r.start], &t[r.end..])
                }
                None => format!("{}, \"Default Program\" : {p} {}", t[..close].trim_end(), &t[close..]),
            }
        }
    };
    let mut out = prefix.to_vec();
    out.extend_from_slice(body.as_bytes());
    Ok(out)
}

/// In a JSON object's text: the byte range of top-level `key`'s scalar value (None when absent or not a scalar) and
/// the position of the object's closing brace. None when the text is not one object.
fn json_top_level(t: &str, key: &str) -> Option<(Option<Range<usize>>, usize)> {
    let b = t.as_bytes();
    let skip_ws = |mut j: usize| {
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        j
    };
    let mut i = skip_ws(0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut found = None;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let s = i + 1;
                let mut e = s;
                while e < b.len() && b[e] != b'"' {
                    e += if b[e] == b'\\' { 2 } else { 1 };
                }
                if e >= b.len() {
                    return None;
                }
                i = e + 1;
                if depth == 1 && found.is_none() && &t[s..e] == key {
                    let j = skip_ws(i);
                    if b.get(j) == Some(&b':') {
                        let vs = skip_ws(j + 1);
                        let mut ve = vs;
                        while ve < b.len() && !matches!(b[ve], b',' | b'}' | b']') && !b[ve].is_ascii_whitespace() {
                            ve += 1;
                        }
                        if ve > vs && !matches!(b[vs], b'"' | b'{' | b'[') {
                            found = Some(vs..ve);
                        }
                    }
                }
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((found, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---- JUCE containers ---------------------------------------------------------------------------

fn be32(b: &[u8], at: usize) -> Result<u32> {
    let s = b.get(at..at + 4).ok_or_else(|| anyhow!("truncated at byte {at}"))?;
    Ok(u32::from_be_bytes(s.try_into()?))
}

/// JUCE's VST2-compatible component state: a `VstW` header and an opaque-chunk fxBank (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Vst2 {
    /// `VstW` version (1).
    wrapper_version: u32,
    bypass: u32,
    /// fxBank version (1 or 2).
    version: u32,
    fx_id: [u8; 4],
    fx_version: u32,
    programs: u32,
    reserved: Vec<u8>,
    chunk: Vec<u8>,
}

/// fxBank fields before the chunk size, after `CcnK` and the byte size.
const BANK_HEAD: usize = 4 + 4 + 4 + 4 + 4 + 128;

impl Vst2 {
    fn parse(b: &[u8]) -> Result<Vst2> {
        if b.get(..4) != Some(b"VstW") {
            bail!("no VstW header");
        }
        let header = be32(b, 4)? as usize;
        if !(8..=256).contains(&header) {
            bail!("VstW header size {header}");
        }
        let (wrapper_version, bypass) = (be32(b, 8)?, be32(b, 12)?);
        let c = 8 + header;
        if b.get(c..c + 4) != Some(b"CcnK") {
            bail!("no CcnK bank after the VstW header");
        }
        let h = c + 8;
        if b.get(h..h + 4) != Some(b"FBCh") {
            bail!("the bank is not an opaque chunk (FBCh)");
        }
        let fx_id: [u8; 4] = b.get(h + 8..h + 12).ok_or_else(|| anyhow!("truncated bank"))?.try_into()?;
        let n = be32(b, h + BANK_HEAD)? as usize;
        let start = h + BANK_HEAD + 4;
        let chunk = start.checked_add(n).and_then(|e| b.get(start..e)).ok_or_else(|| anyhow!("the chunk ({n} bytes) is truncated"))?;
        Ok(Vst2 {
            wrapper_version,
            bypass,
            version: be32(b, h + 4)?,
            fx_id,
            fx_version: be32(b, h + 12)?,
            programs: be32(b, h + 16)?,
            reserved: b[h + 20..h + BANK_HEAD].to_vec(),
            chunk: chunk.to_vec(),
        })
    }

    fn to_bytes(&self) -> Result<Vec<u8>> {
        let n = u32::try_from(self.chunk.len()).ok().filter(|&n| n <= i32::MAX as u32 - 512).ok_or_else(|| anyhow!("state too large"))?;
        let mut out = Vec::with_capacity(16 + 8 + BANK_HEAD + 4 + self.chunk.len());
        out.extend_from_slice(b"VstW");
        for v in [8, self.wrapper_version, self.bypass] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(b"CcnK");
        // the bank's size counts from after this field
        out.extend_from_slice(&((BANK_HEAD + 4) as u32 + n).to_be_bytes());
        out.extend_from_slice(b"FBCh");
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.fx_id);
        out.extend_from_slice(&self.fx_version.to_be_bytes());
        out.extend_from_slice(&self.programs.to_be_bytes());
        let mut reserved = self.reserved.clone();
        reserved.resize(128, 0);
        out.extend_from_slice(&reserved);
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&self.chunk);
        Ok(out)
    }
}

/// Split a JUCE plugin's own data from the private data JUCE appends to it (empty when there is none).
fn split_private(b: &[u8]) -> (&[u8], &[u8]) {
    let m = JUCE_PRIVATE.len();
    if b.len() >= m + 16 && b.ends_with(JUCE_PRIVATE) {
        let at = b.len() - m - 8;
        let size = u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or_default());
        if let Some(total) = size.checked_add(m as u64 + 16).filter(|&t| t <= b.len() as u64) {
            return b.split_at(b.len() - total as usize);
        }
    }
    (b, &[])
}

/// A note (pitch, velocity) the plugin's state certainly plays, to tell when a plugin that loads in the background
/// is ready: for Decent Sampler the root of the sample in the middle of its mapping. None when the state doesn't say.
pub fn probe_note(family: Family, component: &[u8]) -> Option<(u8, u8)> {
    if family != Family::DecentSampler {
        return None;
    }
    let xml = xml_from_binary(split_private(component).0).ok()?;
    let mut samples: Vec<(u8, u8)> = vec![];
    let mut rest = xml;
    while let Some(at) = rest.find("<sample") {
        rest = &rest[at + 7..];
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let tag = &rest[..rest.find('>').unwrap_or(rest.len())];
        let num = |name: &str| tag_attr(tag, name).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v.round().clamp(0.0, 127.0) as u8);
        let Some(pitch) = num("rootNote").or_else(|| num("loNote")) else { continue };
        let (lo, hi) = (num("loVel").unwrap_or(1).max(1), num("hiVel").unwrap_or(127));
        samples.push((pitch, 100.clamp(lo, hi.max(lo))));
    }
    samples.sort_unstable();
    samples.get(samples.len() / 2).copied()
}

/// An attribute's raw value in the text of a start tag (`name="value"` or `name='value'`).
fn tag_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(at) = rest.find(name) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + name.len()..].trim_start();
        rest = &rest[at + name.len()..];
        if before.is_some_and(|c| !c.is_ascii_whitespace()) {
            continue;
        }
        let Some(v) = after.strip_prefix('=') else { continue };
        let v = v.trim_start();
        let q = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        return v[1..].find(q).map(|e| &v[1..1 + e]);
    }
    None
}

/// The XML text of JUCE's `copyXmlToBinary` data.
fn xml_from_binary(b: &[u8]) -> Result<&str> {
    if b.len() <= 8 || &b[..4] != b"VC2!" {
        bail!("no VC2! XML block");
    }
    let n = u32::from_le_bytes(b[4..8].try_into()?) as usize;
    let s = &b[8..8 + n.min(b.len() - 8)];
    let s = &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())];
    std::str::from_utf8(s).map_err(|e| anyhow!("the XML is not UTF-8 ({e})"))
}

/// JUCE's `copyXmlToBinary` encoding of XML text.
fn xml_to_binary(xml: &str) -> Result<Vec<u8>> {
    let n = u32::try_from(xml.len()).ok().filter(|&n| n <= i32::MAX as u32 - 512).ok_or_else(|| anyhow!("XML too large"))?;
    let mut out = Vec::with_capacity(xml.len() + 9);
    out.extend_from_slice(b"VC2!");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(xml.as_bytes());
    out.push(0);
    Ok(out)
}

// ---- XML ---------------------------------------------------------------------------------------

/// An attribute as written: its value is still escaped.
#[derive(Debug, PartialEq, Eq)]
struct Attr<'a> {
    name: &'a str,
    raw: &'a str,
    quote: char,
}

impl Attr<'_> {
    fn write(&self, out: &mut String) {
        out.push(' ');
        out.push_str(self.name);
        out.push('=');
        out.push(self.quote);
        out.push_str(self.raw);
        out.push(self.quote);
    }

    fn write_new(name: &str, value: &str, out: &mut String) {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        for c in value.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                c if (c as u32) < 0x20 => out.push_str(&format!("&#{};", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }
}

/// The root element's start tag: name, attributes, where it ends and whether it is empty (`/>`).
#[derive(Debug)]
struct StartTag<'a> {
    name: &'a str,
    attrs: Vec<Attr<'a>>,
    /// Byte offset just past the tag's `>`.
    end: usize,
    empty: bool,
}

impl StartTag<'_> {
    fn has(&self, name: &str) -> bool {
        self.attrs.iter().any(|a| a.name == name)
    }
}

/// What follows the root's start tag: its end, then the children and end tag unless it is empty.
fn after_tag(t: &str, tag: &StartTag) -> String {
    if tag.empty { "/>".into() } else { format!(">{}", t[tag.end..].trim_end()) }
}

/// Find and read the root element's start tag, skipping the XML declaration, comments and a DOCTYPE.
fn root_tag(t: &str) -> Result<StartTag<'_>> {
    let b = t.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let rest = &t[i..];
        let skip = if rest.starts_with("<?") {
            rest.find("?>").map(|e| e + 2)
        } else if rest.starts_with("<!--") {
            rest.find("-->").map(|e| e + 3)
        } else if rest.starts_with("<!") {
            rest.find('>').map(|e| e + 1)
        } else {
            break;
        };
        i += skip.ok_or_else(|| anyhow!("unterminated XML prolog"))?;
    }
    if b.get(i) != Some(&b'<') {
        bail!("no XML root element");
    }
    let is_name = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':') || c >= 0x80;
    let ns = i + 1;
    let mut j = ns;
    while j < b.len() && is_name(b[j]) {
        j += 1;
    }
    if j == ns {
        bail!("no XML root element");
    }
    let name = &t[ns..j];
    let mut attrs = vec![];
    loop {
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        match b.get(j) {
            None => bail!("the <{name}> tag is not closed"),
            Some(b'>') => return Ok(StartTag { name, attrs, end: j + 1, empty: false }),
            Some(b'/') if b.get(j + 1) == Some(&b'>') => return Ok(StartTag { name, attrs, end: j + 2, empty: true }),
            _ => {}
        }
        let an = j;
        while j < b.len() && is_name(b[j]) {
            j += 1;
        }
        if j == an {
            bail!("bad attribute in <{name}> at byte {j}");
        }
        let aname = &t[an..j];
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) != Some(&b'=') {
            bail!("attribute {aname} in <{name}> has no value");
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let quote = match b.get(j) {
            Some(&q @ (b'"' | b'\'')) => q,
            _ => bail!("attribute {aname} in <{name}> is not quoted"),
        };
        let vs = j + 1;
        let ve = t[vs..].find(quote as char).map(|e| vs + e).ok_or_else(|| anyhow!("attribute {aname} in <{name}> is not closed"))?;
        if t[vs..ve].contains('<') {
            bail!("attribute {aname} in <{name}> contains '<'");
        }
        attrs.push(Attr { name: aname, raw: &t[vs..ve], quote: quote as char });
        j = ve + 1;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_decent_sampler_probe_note_is_a_mapped_sample_from_the_middle() {
        let xml = format!(
            "{XML_DECL} <DecentSampler _samplePath=\"\"><groups><group><sample rootNote=\"48\" loNote=\"0\" hiNote=\"50\" path=\"a.wav\"/>             <sample path='b.wav' rootNote='60' loVel=\"1\" hiVel=\"80\"/><sample rootNote=\"72\" path=\"c.wav\"/></group></groups></DecentSampler>"
        );
        let b = xml_to_binary(&xml).unwrap();
        assert_eq!(probe_note(Family::DecentSampler, &b), Some((60, 80)));
        assert_eq!(probe_note(Family::Vital, &b), None);
        let empty = xml_to_binary(&format!("{XML_DECL} <DecentSampler><groups/></DecentSampler>")).unwrap();
        assert_eq!(probe_note(Family::DecentSampler, &empty), None);
        assert!(Family::DecentSampler.loads_in_background() && !Family::Vital.loads_in_background());
        assert_eq!(tag_attr(r#" path="x" xrootNote="1" rootNote = '7'"#, "rootNote"), Some("7"));
    }

    use super::*;

    const STATES: &str = "D:/baton-build/states";

    /// A dumped default state, or None (with a note) when this machine doesn't have it.
    fn dumped(name: &str) -> Option<Vec<u8>> {
        let p = Path::new(STATES).join(name);
        match std::fs::read(&p) {
            Ok(b) => Some(b),
            Err(_) => {
                eprintln!("skipping: {} not found", p.display());
                None
            }
        }
    }

    fn preset(path: &str, program: Option<usize>) -> VendorPreset {
        let path = PathBuf::from(path);
        VendorPreset { name: path.file_stem().unwrap().to_string_lossy().into_owned(), group: String::new(), path, program }
    }

    /// A JUCE private data block holding `Bypass = false`, as JUCE writes it.
    fn private() -> Vec<u8> {
        let mut tree = b"JUCEPrivateData\0\x01\x01Bypass\0\x01\x01\x03\x00".to_vec();
        let mut out = vec![0u8; 8];
        let n = tree.len() as u64;
        out.append(&mut tree);
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(JUCE_PRIVATE);
        out
    }

    fn vst2(fx_id: &[u8; 4], chunk: Vec<u8>) -> Vec<u8> {
        Vst2 { wrapper_version: 1, bypass: 0, version: 2, fx_id: *fx_id, fx_version: 0x10007, programs: 0, reserved: vec![0; 128], chunk }.to_bytes().unwrap()
    }

    #[test]
    fn families() {
        assert_eq!(family("Vital", "Vital Audio"), Some(Family::Vital));
        assert_eq!(family("ValhallaVintageVerb", "Valhalla DSP, LLC"), Some(Family::Valhalla));
        assert_eq!(family("DecentSampler", "Decidedly"), Some(Family::DecentSampler));
        assert_eq!(family("Ignite - NadIR", "STL Tones"), Some(Family::StlTones));
        assert_eq!(family("Surge XT", "Surge Synth Team"), None);
        assert!(stl_app_matches("Ignite - NadIR", "NadIR"));
        assert!(stl_app_matches("Emissary", "emissary"));
        assert!(!stl_app_matches("Ignite - Emissary", "NadIR"));
        assert!(!stl_app_matches("SuperNadIR", "NadIR"));
    }

    #[test]
    fn vst2_wrapper() {
        let b = vst2(b"Vita", b"{}\0".to_vec());
        assert_eq!(&b[..4], b"VstW");
        assert_eq!(&b[16..20], b"CcnK");
        assert_eq!(u32::from_be_bytes(b[20..24].try_into().unwrap()) as usize, b.len() - 24);
        assert_eq!(&b[32..36], b"Vita");
        assert_eq!(b.len(), 16 + 160 + 3);
        let w = Vst2::parse(&b).unwrap();
        assert_eq!((w.fx_id, w.chunk.as_slice()), (*b"Vita", &b"{}\0"[..]));
        assert_eq!(w.to_bytes().unwrap(), b);
        // truncations and garbage are errors, never panics
        for n in 0..b.len() {
            assert!(Vst2::parse(&b[..n]).is_err(), "{n}");
        }
        let mut huge = b.clone();
        huge[172..176].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Vst2::parse(&huge).is_err());
        let mut hdr = b.clone();
        hdr[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Vst2::parse(&hdr).is_err());
    }

    #[test]
    fn juce_blocks() {
        let x = xml_to_binary("<a b=\"1\"/>").unwrap();
        assert_eq!(&x[..8], b"VC2!\x0a\0\0\0");
        assert_eq!(*x.last().unwrap(), 0);
        assert_eq!(xml_from_binary(&x).unwrap(), "<a b=\"1\"/>");
        assert!(xml_from_binary(b"VC2!").is_err());
        assert!(xml_from_binary(b"VC2!\xff\xff\xff\xff<a/>").is_ok());
        let mut with = x.clone();
        with.extend_from_slice(&private());
        assert_eq!(split_private(&with), (&x[..], &private()[..]));
        assert_eq!(split_private(&x), (&x[..], &[][..]));
        let mut bad = x.clone();
        bad.extend_from_slice(&u64::MAX.to_le_bytes());
        bad.extend_from_slice(JUCE_PRIVATE);
        assert_eq!(split_private(&bad).1.len(), 0);
        assert_eq!(split_private(JUCE_PRIVATE), (JUCE_PRIVATE, &[][..]));
    }

    #[test]
    fn xml_root() {
        let t = "\u{feff}<?xml version=\"1.0\"?>\r\n<!-- hi -->\n<Root a=\"1\" b = 'x&amp;\"y' c=\"\"/>\n";
        let r = root_tag(t.trim_start_matches('\u{feff}')).unwrap();
        assert_eq!(r.name, "Root");
        assert!(r.empty);
        assert_eq!(r.attrs, vec![Attr { name: "a", raw: "1", quote: '"' }, Attr { name: "b", raw: "x&amp;\"y", quote: '\'' }, Attr { name: "c", raw: "", quote: '"' }]);
        let mut s = String::new();
        r.attrs[1].write(&mut s);
        Attr::write_new("p", "C:\\a & <b> \"c\"\n", &mut s);
        assert_eq!(s, " b='x&amp;\"y' p=\"C:\\a &amp; &lt;b&gt; &quot;c&quot;&#10;\"");
        let t2 = "<DecentSampler minVersion=\"1\"><groups/></DecentSampler>";
        let r2 = root_tag(t2).unwrap();
        assert!(!r2.empty);
        assert_eq!(&t2[r2.end..], "<groups/></DecentSampler>");
        for bad in ["", "text", "<", "<a b>", "<a b=1/>", "<a b=\"1/>", "<?xml", "<a b=\"<\"/>"] {
            assert!(root_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn json_keys() {
        let t = r#"{ "Name" : "Default Program", "Programs" : [ { "Default Program" : 3 } ], "Default Program" : 5 }"#;
        let (v, close) = json_top_level(t, "Default Program").unwrap();
        assert_eq!(&t[v.unwrap()], "5");
        assert_eq!(close, t.len() - 1);
        let (v, _) = json_top_level(r#"{"a":"}{\"","b":1}"#, "Default Program").unwrap();
        assert!(v.is_none());
        assert!(json_top_level("[1]", "a").is_none());
        assert!(json_top_level("{\"a\":1", "a").is_none());
    }

    #[test]
    fn synthetic_vital() {
        let mut chunk = br#"{"preset_name":"x","settings":{}}"#.to_vec();
        chunk.push(0);
        chunk.extend_from_slice(&private());
        let template = vst2(b"Vita", chunk);
        let p = preset("C:/presets/Big Pad.vital", None);
        let s = from_bytes(Family::Vital, &template, br#"{"author":"a","settings":{"osc_1_on":1.0}}"#, &p).unwrap();
        let w = Vst2::parse(&s).unwrap();
        assert_eq!(w.fx_id, *b"Vita");
        let (own, priv_) = split_private(&w.chunk);
        assert_eq!(priv_, &private()[..]);
        assert_eq!(own, &b"{\"preset_name\":\"Big Pad\",\"author\":\"a\",\"settings\":{\"osc_1_on\":1.0}}\0"[..]);
        assert!(from_bytes(Family::Vital, &template, b"{\"author\":1}", &p).is_err());
        assert!(from_bytes(Family::Vital, &template, b"[1]", &p).is_err());
        assert!(from_bytes(Family::Vital, &template, b"\xff\xfe", &p).is_err());
        assert!(from_bytes(Family::Vital, b"junk", b"{\"settings\":{}}", &p).is_err());
    }

    #[test]
    fn synthetic_valhalla() {
        let xml = format!("{XML_DECL} <ValhallaRoom pluginVersion=\"2\" presetName=\"Default\" Mix=\"0.5\" Decay=\"0.3\" uiWidth=\"800\"/>");
        let mut chunk = xml_to_binary(&xml).unwrap();
        chunk.extend_from_slice(&private());
        let template = vst2(b"vRm2", chunk);
        let p = preset("C:/x/Hall.vpreset", None);
        let s = from_bytes(Family::Valhalla, &template, b"<?xml version=\"1.0\"?>\n<ValhallaRoom pluginVersion=\"1\" Mix='0.25'\n Decay=\"0.9\"/>\n", &p).unwrap();
        let w = Vst2::parse(&s).unwrap();
        let (own, priv_) = split_private(&w.chunk);
        assert_eq!(priv_, &private()[..]);
        assert_eq!(
            xml_from_binary(own).unwrap(),
            format!("{XML_DECL} <ValhallaRoom pluginVersion=\"1\" Mix='0.25' Decay=\"0.9\" presetName=\"Hall\" uiWidth=\"800\"/>")
        );
        let e = from_bytes(Family::Valhalla, &template, b"<ValhallaShimmer Mix=\"1\"/>", &p).unwrap_err();
        assert!(e.to_string().contains("ValhallaShimmer"), "{e}");
        assert!(from_bytes(Family::Valhalla, &template, b"not xml", &p).is_err());
    }

    #[test]
    fn synthetic_decent() {
        let xml = format!("{XML_DECL} <DecentSampler _presetName=\"Decent Sampler\" _samplePath=\"\" _userVolume=\"0.8\"><ui/><groups/></DecentSampler>");
        let mut template = xml_to_binary(&xml).unwrap();
        template.extend_from_slice(&private());
        let p = preset("C:/libs/A & B/Piano.dspreset", None);
        let s = from_bytes(Family::DecentSampler, &template, b"<DecentSampler minVersion=\"1.0\" _samplePath=\"evil\">\n<groups><group><sample path=\"S/a.wav\"/></group></groups>\n</DecentSampler>\n", &p).unwrap();
        let (own, priv_) = split_private(&s);
        assert_eq!(priv_, &private()[..]);
        let x = xml_from_binary(own).unwrap();
        assert!(x.starts_with(&format!("{XML_DECL} <DecentSampler minVersion=\"1.0\" _presetName=\"Piano\" _samplePath=\"")), "{x}");
        assert!(x.contains("A &amp; B") && !x.contains("evil") && x.contains("_userVolume=\"0.8\""), "{x}");
        assert!(x.ends_with("<groups><group><sample path=\"S/a.wav\"/></group></groups>\n</DecentSampler>"), "{x}");
        assert!(from_bytes(Family::DecentSampler, &template, b"<NotDecent/>", &p).is_err());
        assert!(to_state(Family::DecentSampler, &template, &preset("C:/x/lib.dslibrary", None)).unwrap_err().to_string().contains("dslibrary"));
    }

    #[test]
    fn synthetic_stl() {
        let bank = r#"{ "Name" : "Default", "Application" : { "Name" : "NadIR" }, "Programs" : [ { "Name" : "A", "Parameters" : [ { "HPF0" : 0 } ] }, { "Name" : "B", "Parameters" : [ ] } ], "Default Program" : 0 }"#;
        let template = format!("0{bank}");
        let p = |i| preset("C:/STL/Banks/NadIR Default.json", i);
        let s = from_bytes(Family::StlTones, template.as_bytes(), bank.as_bytes(), &p(Some(1))).unwrap();
        assert_eq!(String::from_utf8(s).unwrap(), template.replace("\"Default Program\" : 0 }", "\"Default Program\" : 1 }"));
        let s = from_bytes(Family::StlTones, template.as_bytes(), bank.as_bytes(), &p(None)).unwrap();
        assert_eq!(String::from_utf8(s).unwrap(), template);
        // no current program yet; tight formatting
        let tight = r#"{"Application":{"Name":"NadIR"},"Programs":[{"Name":"A"},{"Name":"B"}],"Default Program":0}"#;
        let s = String::from_utf8(from_bytes(Family::StlTones, template.as_bytes(), tight.as_bytes(), &p(Some(1))).unwrap()).unwrap();
        assert!(s.ends_with("\"Default Program\":1 }"), "{s}");
        let none = r#"{"Application":{"Name":"NadIR"},"Programs":[{"Name":"A"},{"Name":"B"}]}"#;
        let s = String::from_utf8(from_bytes(Family::StlTones, template.as_bytes(), none.as_bytes(), &p(Some(1))).unwrap()).unwrap();
        assert!(s.ends_with("}], \"Default Program\" : 1 }"), "{s}");
        assert!(from_bytes(Family::StlTones, template.as_bytes(), bank.as_bytes(), &p(Some(2))).is_err());
        let other = bank.replace("NadIR", "Emissary");
        assert!(from_bytes(Family::StlTones, template.as_bytes(), other.as_bytes(), &p(Some(0))).unwrap_err().to_string().contains("Emissary"));
        assert!(from_bytes(Family::StlTones, bank.as_bytes(), bank.as_bytes(), &p(Some(0))).is_err());
    }

    #[test]
    fn untrusted_input_never_panics() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let samples: [&[u8]; 6] = [
            b"<ValhallaRoom Mix=\"1\"/>",
            b"<DecentSampler><groups/></DecentSampler>",
            b"{\"settings\":{},\"preset_name\":\"x\"}",
            b"{ \"Application\" : { \"Name\" : \"NadIR\" }, \"Programs\" : [ { \"Name\" : \"A\" } ], \"Default Program\" : 0 }",
            b"VstW\0\0\0\x08",
            b"VC2!\x05\0\0\0<a/>\0",
        ];
        let p = preset("C:/x/y.z", Some(0));
        for _ in 0..3000 {
            let mut d = samples[(next() % samples.len() as u64) as usize].to_vec();
            for _ in 0..(next() % 4) {
                if d.is_empty() {
                    break;
                }
                let i = (next() % d.len() as u64) as usize;
                match next() % 3 {
                    0 => d[i] = next() as u8,
                    1 => d.truncate(i),
                    _ => d.insert(i, b"<>\"'{}[]:,\0\\"[(next() % 12) as usize]),
                }
            }
            for f in [Family::Vital, Family::Valhalla, Family::DecentSampler, Family::StlTones] {
                let _ = from_bytes(f, &d, &d, &p);
                let _ = from_bytes(f, samples[(next() % samples.len() as u64) as usize], &d, &p);
            }
            let _ = Vst2::parse(&d);
            let _ = split_private(&d);
            let _ = xml_from_binary(&d);
        }
    }

    #[test]
    fn listing_skips_links_and_depth() {
        let root = std::env::temp_dir().join(format!("flux-vendor-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let deep = (0..MAX_DEPTH + 2).fold(root.join("Pack").join("Presets"), |p, i| p.join(format!("d{i}")));
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("Pack").join("Presets").join("b.vital"), b"{}").unwrap();
        std::fs::write(root.join("Pack").join("Presets").join("A.VITAL"), b"{}").unwrap();
        std::fs::write(root.join("Pack").join("Presets").join("notes.txt"), b"").unwrap();
        std::fs::write(deep.join("too deep.vital"), b"{}").unwrap();
        // a link to a folder outside the root is not followed (creating one may need privileges on Windows)
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("outside.vital"), b"{}").unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, root.join("Pack").join("link")).is_ok()
            || std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(root.join("Pack").join("junction"))
                .arg(&outside)
                .output()
                .is_ok_and(|o| o.status.success());
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, root.join("Pack").join("link")).is_ok();
        if !linked {
            eprintln!("note: could not create a folder link; link check skipped");
        }
        // overlapping roots list a file once
        let got = list_in(Family::Vital, "Vital", &[root.clone(), root.join("Pack")]);
        let names: Vec<(&str, &str)> = got.iter().map(|p| (p.group.as_str(), p.name.as_str())).collect();
        assert_eq!(names, vec![("Pack", "A"), ("Pack", "b")]);
        let small = root.join("Pack").join("Presets").join("b.vital");
        assert_eq!(read_limited(&small, 2).unwrap(), b"{}");
        assert!(read_limited(&small, 1).unwrap_err().to_string().contains("larger"));
        assert!(read_limited(&root, 10).is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn decent_sample_path_is_the_preset_folder() {
        let dir = std::env::temp_dir().join(format!("flux-vendor-ds-{}", std::process::id())).join("Lib & Co");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Keys.dspreset");
        std::fs::write(&file, b"\xEF\xBB\xBF<?xml version=\"1.0\"?>\n<DecentSampler><groups><group><sample path=\"Samples/k.wav\"/></group></groups></DecentSampler>").unwrap();
        let template = xml_to_binary(&format!("{XML_DECL} <DecentSampler _presetName=\"\" _samplePath=\"\"><ui/></DecentSampler>")).unwrap();
        let s = to_state(Family::DecentSampler, &template, &VendorPreset { name: "Keys".into(), group: String::new(), path: file, program: None }).unwrap();
        let x = xml_from_binary(&s).unwrap();
        let tag = root_tag(x).unwrap();
        let sp = tag.attrs.iter().find(|a| a.name == "_samplePath").unwrap().raw.replace("&amp;", "&");
        assert_eq!(Path::new(&sp), std::path::absolute(&dir).unwrap());
        assert!(x.ends_with("<groups><group><sample path=\"Samples/k.wav\"/></group></groups></DecentSampler>"), "{x}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn dumped_states_round_trip() {
        for (file, family) in [("Vital.bin", Family::Vital), ("ValhallaSupermassive.bin", Family::Valhalla)] {
            let Some(b) = dumped(file) else { continue };
            let w = Vst2::parse(&b).unwrap();
            assert_eq!(w.to_bytes().unwrap(), b, "{file}");
            let (own, private) = split_private(&w.chunk);
            assert!(private.ends_with(JUCE_PRIVATE), "{file}");
            if family == Family::Valhalla {
                assert_eq!(&w.fx_id, b"sMas");
                let x = xml_from_binary(own).unwrap();
                assert_eq!(xml_to_binary(x).unwrap(), own);
                assert_eq!(root_tag(x).unwrap().name, "ValhallaSupermassive");
            } else {
                assert_eq!(&w.fx_id, b"Vita");
                assert_eq!(own.last(), Some(&0));
            }
        }
        if let Some(b) = dumped("DecentSampler.bin") {
            let (own, private) = split_private(&b);
            assert!(private.ends_with(JUCE_PRIVATE));
            let x = xml_from_binary(own).unwrap();
            assert_eq!(xml_to_binary(x).unwrap(), own);
            assert!(root_tag(x).unwrap().has("_samplePath"));
        }
        for file in ["Emissary.bin", "NadIR.bin"] {
            let Some(b) = dumped(file) else { continue };
            let bank = stl_bank(&b[1..]).unwrap();
            assert!(!bank.programs.is_empty());
            // selecting the current program again changes nothing
            let cur = json_top_level(std::str::from_utf8(&b[1..]).unwrap(), "Default Program").unwrap().0.unwrap();
            let n: usize = std::str::from_utf8(&b[1 + cur.start..1 + cur.end]).unwrap().parse().unwrap();
            assert_eq!(stl_state(&b, &b[1..], Some(n)).unwrap(), b, "{file}");
        }
    }

    #[test]
    fn real_presets() {
        let docs = documents();
        if let Some(t) = dumped("Vital.bin") {
            let roots: Vec<PathBuf> = docs.iter().map(|d| d.join("Vital")).filter(|d| d.is_dir()).collect();
            let all = list_in(Family::Vital, "Vital", &roots);
            assert!(roots.is_empty() || !all.is_empty());
            for p in all.iter().take(3) {
                let s = to_state(Family::Vital, &t, p).unwrap();
                let w = Vst2::parse(&s).unwrap();
                assert!(w.chunk.starts_with(b"{"), "{}", p.path.display());
            }
        }
        if let Some(t) = dumped("ValhallaSupermassive.bin") {
            let all = list(Family::Valhalla, "ValhallaSupermassive");
            assert!(dirs(Family::Valhalla, "ValhallaSupermassive").is_empty() || !all.is_empty());
            for p in all.iter().take(5) {
                let s = to_state(Family::Valhalla, &t, p).unwrap();
                let w = Vst2::parse(&s).unwrap();
                let x = xml_from_binary(split_private(&w.chunk).0).unwrap();
                assert!(root_tag(x).unwrap().has("Mix") && x.contains("uiWidth"), "{}", p.path.display());
            }
        }
        for (file, name) in [("Emissary.bin", "Ignite - Emissary"), ("NadIR.bin", "Ignite - NadIR")] {
            let Some(t) = dumped(file) else { continue };
            let all = list(Family::StlTones, name);
            assert!(all.iter().all(|p| p.group.contains(name.trim_start_matches("Ignite - "))), "{all:?}");
            for p in &all {
                let s = to_state(Family::StlTones, &t, p).unwrap();
                assert_eq!(s[0], b'0');
                let v = json_top_level(std::str::from_utf8(&s[1..]).unwrap(), "Default Program").unwrap().0.unwrap();
                assert_eq!(s[1 + v.start..1 + v.end], *p.program.unwrap().to_string().as_bytes());
            }
        }
    }
}
