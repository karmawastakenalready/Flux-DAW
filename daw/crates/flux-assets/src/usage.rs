//! Which assets the current projects use.
//!
//! Starting from the manifest's `usage_roots` (Flux projects, rig files, SFZ wrappers, pipeline scripts), every
//! referenced path is resolved and followed: JSON files contribute all their string values, SFZ files their
//! `sample=`, `default_path=` and `#include` references (with `#define` variables), saved plugin states
//! (`.vstpreset`) the absolute paths written inside them (a sampler's library folder), and Python files their string
//! literals (a heuristic: a literal that names an asset folder counts as a use). A reference that lands inside an
//! asset folder, or inside an install location of an asset installed by another tool (a plugin in the VST3 folder),
//! marks that asset as used by the root file it was reached from. An absolute path that ends in an asset folder of
//! another checkout (a project saved on another machine) counts too, as a heuristic use.

use crate::fsutil;
use crate::glob;
use crate::manifest::{Loaded, Source};
use crate::places::Places;
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

/// How deep references are followed from a root (project -> wrapper SFZ -> included SFZ ...).
const MAX_DEPTH: usize = 8;
/// Text files larger than this are not scanned.
const MAX_FILE_BYTES: u64 = 16 << 20;

/// One use of an asset.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct UsageRef {
    /// The usage root (relative to the project root) the reference was reached from.
    pub root: String,
    /// The file that contains the reference, when it is not the root itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Found by the Python string heuristic rather than a resolved path.
    pub heuristic: bool,
}

/// Result of a usage scan.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    /// Asset id -> uses (sorted, deduplicated per root).
    pub by_asset: BTreeMap<String, Vec<UsageRef>>,
    /// Usage roots that were scanned.
    pub roots: Vec<String>,
    /// Files followed from the roots.
    pub files_scanned: usize,
}

impl Usage {
    pub fn uses(&self, id: &str) -> &[UsageRef] {
        self.by_asset.get(id).map_or(&[], Vec::as_slice)
    }

    pub fn is_used(&self, id: &str) -> bool {
        !self.uses(id).is_empty()
    }

    /// Distinct root files that use the asset.
    pub fn roots_using(&self, id: &str) -> Vec<&str> {
        let s: BTreeSet<&str> = self.uses(id).iter().map(|u| u.root.as_str()).collect();
        s.into_iter().collect()
    }
}

struct AssetDir {
    id: String,
    /// Normalised lowercase absolute path with a trailing separator, for prefix tests.
    prefix: String,
    /// The folder below the project root (`None` for an install location outside it): its name, for the Python
    /// heuristic, and its relative path.
    rel: Option<(String, String)>,
}

fn key(p: &Path) -> String {
    let s = fsutil::normalize_lexically(p).to_string_lossy().replace('\\', "/");
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// Scan the usage roots of the manifest below `root`; install locations of assets installed by another tool are
/// looked for in `places`.
pub fn scan(root: &Path, loaded: &Loaded, places: &Places) -> Result<Usage> {
    scan_with(root, loaded, places, &[])
}

/// [`scan`], also following `extra` files outside the root (an installed Flux keeps its projects in Documents).
pub fn scan_with(root: &Path, loaded: &Loaded, places: &Places, extra: &[PathBuf]) -> Result<Usage> {
    let mut dirs: Vec<AssetDir> = Vec::new();
    for a in loaded.assets() {
        if let Some(rel) = &a.path {
            let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
            dirs.push(AssetDir { id: a.id.clone(), prefix: format!("{}/", key(&root.join(rel))), rel: Some((name, rel.clone())) });
        }
        for s in &a.sources {
            if let Source::External(e) = s {
                for p in e.installed.iter().flat_map(|loc| places.expand(loc)) {
                    dirs.push(AssetDir { id: a.id.clone(), prefix: format!("{}/", key(&p)), rel: None });
                }
            }
        }
    }
    let mut roots = BTreeSet::new();
    for pat in &loaded.manifest.policy.usage_roots {
        for f in glob::expand(root, pat)? {
            roots.insert(f);
        }
    }
    for f in extra.iter().filter(|f| f.is_file()) {
        roots.insert(fsutil::relative_to(root, f).unwrap_or_else(|| f.to_string_lossy().replace('\\', "/")));
    }
    let mut usage = Usage { roots: roots.iter().cloned().collect(), ..Default::default() };
    let mut found: BTreeMap<String, BTreeSet<UsageRef>> = BTreeMap::new();
    let mut scanned = HashSet::new();
    for r in &roots {
        let mut queue = VecDeque::from([(root.join(r), 0usize)]);
        let mut visited = HashSet::new();
        while let Some((file, depth)) = queue.pop_front() {
            if !visited.insert(key(&file)) {
                continue;
            }
            scanned.insert(key(&file));
            let via = fsutil::relative_to(root, &file).filter(|v| v != r);
            let refs = references(&file, root);
            for reference in refs {
                match reference {
                    Ref::Path(p) => {
                        let k = key(&p);
                        if let Some(d) = dirs.iter().find(|d| k.starts_with(&d.prefix) || format!("{k}/") == d.prefix) {
                            found.entry(d.id.clone()).or_default().insert(UsageRef { root: r.clone(), via: via.clone(), heuristic: false });
                        } else if depth < MAX_DEPTH && is_followable(&p) && p.is_file() && fsutil::relative_to(root, &p).is_some() {
                            queue.push_back((p, depth + 1));
                        } else if fsutil::relative_to(root, &p).is_none() {
                            // saved in another checkout: the path still names the asset folder
                            let k = format!("{k}/");
                            for d in &dirs {
                                if let Some((_, rel)) = &d.rel
                                    && k.contains(&format!("/{}/", key(Path::new(rel))))
                                {
                                    found.entry(d.id.clone()).or_default().insert(UsageRef { root: r.clone(), via: via.clone(), heuristic: true });
                                }
                            }
                        }
                    }
                    Ref::Literal(s) => {
                        let norm = s.replace('\\', "/");
                        for d in &dirs {
                            if let Some((name, rel)) = &d.rel
                                && (norm.contains(rel.as_str()) || norm.split('/').any(|c| c == name))
                            {
                                found.entry(d.id.clone()).or_default().insert(UsageRef { root: r.clone(), via: via.clone(), heuristic: true });
                            }
                        }
                    }
                }
            }
        }
    }
    usage.files_scanned = scanned.len();
    usage.by_asset = found.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect();
    Ok(usage)
}

fn is_followable(p: &Path) -> bool {
    let n = p.to_string_lossy().to_ascii_lowercase();
    n.ends_with(".sfz") || n.ends_with(".json") || n.ends_with(".sfzh") || n.ends_with(".vstpreset")
}

enum Ref {
    /// A resolved path candidate.
    Path(PathBuf),
    /// A string literal from a script (heuristic matching).
    Literal(String),
}

fn references(file: &Path, project_root: &Path) -> Vec<Ref> {
    let Ok(md) = fs::metadata(file) else { return vec![] };
    if md.len() > MAX_FILE_BYTES {
        return vec![];
    }
    let Ok(bytes) = fs::read(file) else { return vec![] };
    let text = String::from_utf8_lossy(&bytes);
    let dir = fsutil::parent_dir(file).to_path_buf();
    let lower = file.to_string_lossy().to_ascii_lowercase();
    if lower.ends_with(".json") {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![] };
        let mut strings = Vec::new();
        collect_strings(&v, &mut strings);
        let mut out = Vec::new();
        for s in strings.into_iter().filter(|s| looks_like_path(s)) {
            let p = Path::new(&s);
            if p.is_absolute() {
                out.push(Ref::Path(p.to_path_buf()));
            } else {
                // Flux stores paths relative to the project root; also try relative to the file.
                out.push(Ref::Path(project_root.join(p)));
                out.push(Ref::Path(dir.join(p)));
            }
        }
        out
    } else if lower.ends_with(".sfz") || lower.ends_with(".sfzh") {
        sfz_references(&text, &dir).into_iter().map(Ref::Path).collect()
    } else if lower.ends_with(".py") {
        python_literals(&text).into_iter().map(Ref::Literal).collect()
    } else if lower.ends_with(".vstpreset") {
        embedded_paths(&bytes).into_iter().map(Ref::Path).collect()
    } else {
        vec![]
    }
}

fn collect_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

fn looks_like_path(s: &str) -> bool {
    s.len() < 1024 && (s.contains('/') || s.contains('\\')) && !s.contains('\n') && !s.starts_with("http")
}

/// Paths referenced by an SFZ file: samples (with `default_path`), and `#include`d files. `#define`
/// variables are substituted.
pub fn sfz_references(text: &str, dir: &Path) -> Vec<PathBuf> {
    let mut defines: Vec<(String, String)> = Vec::new();
    let mut default_path = String::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = match raw.find("//") {
            Some(i) => &raw[..i],
            None => raw,
        };
        let line = substitute(line, &defines);
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("#define") {
            let mut it = rest.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                defines.retain(|(dk, _)| dk != k);
                defines.push((k.to_string(), v.to_string()));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("#include") {
            let inc = rest.trim().trim_matches('"');
            if !inc.is_empty() {
                out.push(dir.join(inc.replace('\\', "/")));
            }
            continue;
        }
        if let Some(dp) = opcode_value(&line, "default_path") {
            default_path = dp.replace('\\', "/");
        }
        if let Some(s) = opcode_value(&line, "sample") {
            let s = s.replace('\\', "/");
            if !s.starts_with('*') {
                out.push(dir.join(format!("{default_path}{s}")));
            }
        }
    }
    out
}

fn substitute(line: &str, defines: &[(String, String)]) -> String {
    let mut s = line.to_string();
    // Longest names first so `$AB` is not replaced by `$A`.
    let mut d: Vec<&(String, String)> = defines.iter().collect();
    d.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    for (k, v) in d {
        if s.contains(k.as_str()) {
            s = s.replace(k.as_str(), v);
        }
    }
    s
}

/// The value of `key=` on an SFZ line. Values may contain spaces (sample paths); a value ends where the next
/// `name=` opcode or `<header>` begins.
pub fn opcode_value(line: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=");
    let mut search = 0;
    let start = loop {
        let i = line[search..].find(&pat)? + search;
        let before_ok = i == 0 || line[..i].ends_with(|c: char| c.is_whitespace() || c == '>');
        if before_ok {
            break i + pat.len();
        }
        search = i + pat.len();
    };
    let rest = &line[start..];
    let mut end = rest.len();
    if let Some(i) = rest.find('<') {
        end = end.min(i);
    }
    // next opcode: whitespace, identifier, '='
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < end {
        if bytes[i].is_ascii_whitespace() {
            let mut j = i;
            while j < end && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let k0 = j;
            while j < end && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') {
                j += 1;
            }
            if j > k0 && j < end && bytes[j] == b'=' {
                end = i;
                break;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    let v = rest[..end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Absolute paths written inside a binary file (a plugin's saved state): in its text runs (UTF-8, as XML or JSON
/// usually is) and its UTF-16LE runs (wide strings). A path runs from a drive letter (`C:\`, `C:/`), or a `/` that
/// opens a quoted or attribute value, to the next quote, angle bracket or control character; XML entities are
/// decoded, and JSON's doubled backslashes collapse like any repeated separator.
fn embedded_paths(bytes: &[u8]) -> Vec<PathBuf> {
    let mut runs: Vec<String> = Vec::new();
    let mut start = None;
    for (i, &b) in bytes.iter().chain([&0u8]).enumerate() {
        match (b >= 0x20 && b != 0x7f, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s >= 4 {
                    runs.push(String::from_utf8_lossy(&bytes[s..i]).into_owned());
                }
                start = None;
            }
            _ => {}
        }
    }
    for offset in [0, 1] {
        let units: Vec<u16> = bytes.get(offset..).unwrap_or_default().as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        for run in units.split(|&u| u < 0x20 || u == 0x7f) {
            // only runs that can hold a drive path (`:` then a separator); bytes read two at a time make long runs
            if run.len() >= 4 && run.windows(2).any(|w| w[0] == u16::from(b':') && matches!(w[1], 0x2f | 0x5c)) {
                runs.push(String::from_utf16_lossy(run));
            }
        }
    }
    let mut out = Vec::new();
    for run in &runs {
        let c: Vec<char> = run.chars().collect();
        for i in 0..c.len() {
            let opener = if i == 0 { None } else { Some(c[i - 1]) };
            let drive = c[i].is_ascii_alphabetic()
                && c.get(i + 1) == Some(&':')
                && matches!(c.get(i + 2), Some('\\' | '/'))
                && opener.is_none_or(|o| !o.is_alphanumeric());
            let unix = c[i] == '/' && matches!(opener, Some('"' | '\'' | '=' | '>')) && c.get(i + 1).is_some_and(|n| n.is_alphanumeric());
            if !(drive || unix) {
                continue;
            }
            let end = c[i..]
                .iter()
                .position(|&ch| matches!(ch, '"' | '<' | '>' | '|' | '?' | '*') || ch.is_control() || (opener == Some('\'') && ch == '\''))
                .map_or(c.len(), |n| i + n);
            let raw: String = c[i..end].iter().collect();
            let path = raw.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
            let path = path.trim_end();
            if path.len() > 3 {
                out.push(PathBuf::from(path));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Quoted string literals in Python source (single, double and triple quotes; no escapes interpreted).
fn python_literals(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            q @ (b'"' | b'\'') => {
                let triple = i + 2 < b.len() && b[i + 1] == q && b[i + 2] == q;
                let open = if triple { 3 } else { 1 };
                let start = i + open;
                let mut j = start;
                loop {
                    if j >= b.len() {
                        break;
                    }
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if !triple && b[j] == b'\n' {
                        break;
                    }
                    if b[j] == q && (!triple || (j + 2 < b.len() && b[j + 1] == q && b[j + 2] == q)) {
                        break;
                    }
                    j += 1;
                }
                let end = j.min(b.len());
                if let Some(s) = src.get(start..end)
                    && s.len() < 512
                {
                    out.push(s.to_string());
                }
                i = end + open;
            }
            _ => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sfz_opcodes() {
        assert_eq!(opcode_value("<region> sample=Samples/A 1.wav lokey=60", "sample").as_deref(), Some("Samples/A 1.wav"));
        assert_eq!(opcode_value("sample=x.wav", "sample").as_deref(), Some("x.wav"));
        assert_eq!(opcode_value("<region>sample=x.wav<region>sample=y.wav", "sample").as_deref(), Some("x.wav"));
        assert_eq!(opcode_value("<control> default_path=../instruments/V/", "default_path").as_deref(), Some("../instruments/V/"));
        assert_eq!(opcode_value("oversample=2", "sample"), None);
    }

    #[test]
    fn sfz_paths() {
        let text = "#define $DIR Samples\n<control> default_path=../lib/\n<region> sample=$DIR/a.wav // c\n#include \"inc/b.sfz\"\n<region> sample=*sine\n";
        let refs = sfz_references(text, Path::new("/p/sfz"));
        assert_eq!(refs, vec![PathBuf::from("/p/sfz/../lib/Samples/a.wav"), PathBuf::from("/p/sfz/inc/b.sfz")]);
    }

    #[test]
    fn python() {
        let lits = python_literals("x = os.path.join(INST, \"VSCO-2-CE\")  # 'comment'\ny = '''multi\nline'''\nz = 'it\\'s'");
        assert_eq!(lits, ["VSCO-2-CE", "multi\nline", "it\\'s"]);
    }

    #[test]
    fn scans_a_project() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for p in ["instruments/Piano/Samples", "instruments/Drums", "instruments/Unused", "daw/projects", "sfz"] {
            fs::create_dir_all(r.join(p)).unwrap();
        }
        fs::write(r.join("instruments/Piano/Samples/c4.wav"), "").unwrap();
        fs::write(r.join("sfz/wrap.sfz"), "<control> default_path=../instruments/Piano/\n<region> sample=Samples/c4.wav\n").unwrap();
        fs::write(r.join("daw/projects/song.flux.json"), r#"{"tracks":[{"sfz":"sfz/wrap.sfz"},{"sfz":"instruments/Drums/kit.sfz"}]}"#).unwrap();
        fs::write(r.join("pieces.py"), "LIB = 'Unused'\n").unwrap();
        let toml = r#"
schema = 1
[policy]
usage_roots = ["daw/projects/*.flux.json", "*.py"]
[[asset]]
id = "piano"
name = "P"
kind = "sample-library"
scope = "audio"
path = "instruments/Piano"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "vendored"
[[asset]]
id = "drums"
name = "D"
kind = "sample-library"
scope = "audio"
path = "instruments/Drums"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "vendored"
[[asset]]
id = "unused"
name = "U"
kind = "sample-library"
scope = "audio"
path = "instruments/Unused"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "vendored"
"#;
        let l = crate::manifest::parse(toml).unwrap();
        let u = scan(r, &l, &Places::default()).unwrap();
        assert_eq!(u.roots, ["daw/projects/song.flux.json", "pieces.py"]);
        assert_eq!(u.uses("piano"), [UsageRef { root: "daw/projects/song.flux.json".into(), via: Some("sfz/wrap.sfz".into()), heuristic: false }]);
        assert_eq!(u.roots_using("drums"), ["daw/projects/song.flux.json"]);
        assert!(u.uses("unused")[0].heuristic);
    }

    #[test]
    fn paths_inside_binary_states() {
        let b = b"\x00\x01VST3<a p=\"C:\\Libs\\Tom &amp; Jerry\\x\" q='/Users/me/it&apos;s/y' r=\"rel/path\"></a>\x00{\"f\":\"D:\\\\json\\\\z\"}\x00";
        let mut found: Vec<String> = embedded_paths(b).iter().map(|p| p.to_string_lossy().into_owned()).collect();
        found.sort();
        assert_eq!(found, ["/Users/me/it's/y", "C:\\Libs\\Tom & Jerry\\x", "D:\\\\json\\\\z"]);
        let mut wide = vec![0u8, 7];
        "E:\\Wide\\kit".encode_utf16().for_each(|u| wide.extend(u.to_le_bytes()));
        wide.extend([0, 0]);
        assert_eq!(embedded_paths(&wide), [PathBuf::from("E:\\Wide\\kit")]);
        assert_eq!(embedded_paths(&wide[1..]), [PathBuf::from("E:\\Wide\\kit")], "odd offset");
    }

    #[test]
    fn follows_plugin_states_and_install_locations() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let vst3 = tempfile::tempdir().unwrap();
        for p in ["instruments/Bottles/Bottles", "instruments/Wide", "daw/projects", "plugin-states"] {
            fs::create_dir_all(r.join(p)).unwrap();
        }
        fs::create_dir_all(vst3.path().join("Sampler.vst3")).unwrap();
        // a sampler's saved state: a binary header, then XML naming the library folder it loads
        let mut state = b"VST3\x01\x00\x00\x00".to_vec();
        let lib = r.join("instruments").join("Bottles").join("Bottles");
        state.extend(format!("<DecentSampler _samplePath=\"{}\" _userVolume=\"1.0\"/>", lib.display()).as_bytes());
        // and a wide string written on another machine, naming another library
        state.extend([0, 0, 0]);
        "D:\\old\\checkout\\instruments\\Wide\\kit".encode_utf16().for_each(|u| state.extend(u.to_le_bytes()));
        state.extend([0, 0]);
        fs::write(r.join("plugin-states/abc.vstpreset"), state).unwrap();
        let project = serde_json::json!({"tracks": [{"instrument": {
            "kind": "vst3", "path": vst3.path().join("Sampler.vst3"), "state": "plugin-states/abc.vstpreset"}}]});
        fs::write(r.join("daw/projects/song.flux.json"), project.to_string()).unwrap();
        let toml = r#"
schema = 1
[policy]
usage_roots = ["daw/projects/*.flux.json"]
[[asset]]
id = "bottles"
name = "B"
kind = "sample-library"
scope = "audio"
path = "instruments/Bottles"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "vendored"
[[asset]]
id = "wide"
name = "W"
kind = "sample-library"
scope = "audio"
path = "instruments/Wide"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "vendored"
[[asset]]
id = "sampler"
name = "S"
kind = "plugin"
scope = "tool"
version = "1"
license = { id = "MIT" }
[[asset.source]]
type = "external"
page = "https://example.com/sampler"
instructions = "Run its installer."
installed = ["{vst3}/Sampler.vst3"]
[[asset]]
id = "other"
name = "O"
kind = "plugin"
scope = "tool"
version = "1"
license = { id = "MIT" }
[[asset.source]]
type = "external"
page = "https://example.com/other"
instructions = "Run its installer."
installed = ["{vst3}/Other.vst3"]
"#;
        let l = crate::manifest::parse(toml).unwrap();
        let places = Places::default().with("vst3", vec![vst3.path().to_path_buf()]);
        let u = scan(r, &l, &places).unwrap();
        let root = "daw/projects/song.flux.json".to_string();
        let via = Some("plugin-states/abc.vstpreset".to_string());
        assert_eq!(u.uses("bottles"), [UsageRef { root: root.clone(), via: via.clone(), heuristic: false }]);
        assert_eq!(u.uses("wide"), [UsageRef { root: root.clone(), via, heuristic: true }]);
        assert_eq!(u.uses("sampler"), [UsageRef { root, via: None, heuristic: false }]);
        assert!(!u.is_used("other"));
    }
}
