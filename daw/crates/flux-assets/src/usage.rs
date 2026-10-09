//! Which assets the current projects use.
//!
//! Starting from the manifest's `usage_roots` (Flux projects, rig files, SFZ wrappers, pipeline scripts), every
//! referenced path is resolved and followed: JSON files contribute all their string values, SFZ files their
//! `sample=`, `default_path=` and `#include` references (with `#define` variables), and Python files their string
//! literals (a heuristic: a literal that names an asset folder counts as a use). A reference that lands inside an
//! asset folder marks that asset as used by the root file it was reached from.

use crate::fsutil;
use crate::glob;
use crate::manifest::Loaded;
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
    /// Folder name, for the Python heuristic.
    name: String,
    rel: String,
}

fn key(p: &Path) -> String {
    let s = fsutil::normalize_lexically(p).to_string_lossy().replace('\\', "/");
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// Scan the usage roots of the manifest below `root`.
pub fn scan(root: &Path, loaded: &Loaded) -> Result<Usage> {
    let dirs: Vec<AssetDir> = loaded
        .assets()
        .iter()
        .filter_map(|a| {
            let rel = a.path.as_ref()?;
            let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
            Some(AssetDir { id: a.id.clone(), prefix: format!("{}/", key(&root.join(rel))), name, rel: rel.clone() })
        })
        .collect();
    let mut roots = BTreeSet::new();
    for pat in &loaded.manifest.policy.usage_roots {
        for f in glob::expand(root, pat)? {
            roots.insert(f);
        }
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
                        }
                    }
                    Ref::Literal(s) => {
                        let norm = s.replace('\\', "/");
                        for d in &dirs {
                            if norm.contains(&d.rel) || norm.split('/').any(|c| c == d.name) {
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
    n.ends_with(".sfz") || n.ends_with(".json") || n.ends_with(".sfzh")
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
        let u = scan(r, &l).unwrap();
        assert_eq!(u.roots, ["daw/projects/song.flux.json", "pieces.py"]);
        assert_eq!(u.uses("piano"), [UsageRef { root: "daw/projects/song.flux.json".into(), via: Some("sfz/wrap.sfz".into()), heuristic: false }]);
        assert_eq!(u.roots_using("drums"), ["daw/projects/song.flux.json"]);
        assert!(u.uses("unused")[0].heuristic);
    }
}
