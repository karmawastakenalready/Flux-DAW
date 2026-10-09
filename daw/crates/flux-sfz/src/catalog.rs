//! Instrument listings: which SFZ files are programs (they load on their own) and which are fragments (region
//! maps and modules that a program `#include`s), and a one-line summary of a program.
//!
//! A file is a fragment when a program of the same library includes it. The library is the folder under
//! `instruments/` for library files and the first folder under the root otherwise (`sfz/`, `build/`...), so a
//! library program that a hand-made program elsewhere wraps with `#include` is still a program. Includes are
//! followed as sfizz follows them from the including program (relative to its folder, variables expanded), so
//! only programs count as includers: files are read shallowest first, a file already known to be a fragment is
//! not read as a program, and the result is then settled by recomputing what the programs include until nothing
//! changes.
//!
//! Reading a whole collection takes seconds, so what each file includes, and each summary, are cached for the
//! life of the process. A cached result is used only while the modification time and size of every file it
//! was read from are unchanged (and, for summaries, of the folders the samples are in, so samples installed
//! later are noticed).

use crate::audit::check_paths;
use crate::error::Result;
use crate::fsutil::lexical_normalize;
use crate::info::{span_text, spans_text};
use crate::instrument::{Instrument, LoadOptions};
use crate::ranges::{Span, ranges};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::SystemTime;

/// A program in brief.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Regions sfizz plays (a missing sample drops its regions).
    pub regions: usize,
    /// Keys that start a note, e.g. `G3-D6 (55-86)`.
    pub keys: String,
    /// `120 sus, 121 trem...`: keyswitches and their articulations.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub keyswitches: String,
    pub velocity_layers: usize,
    /// Distinct sample files that do not exist.
    pub missing_samples: usize,
    /// Errors in the file (missing samples included).
    pub errors: usize,
}

/// The result of [`classify`].
#[derive(Clone, Debug, Default)]
pub struct Classification {
    /// Fragment (as [`key`]) -> a program that includes it.
    fragments: HashMap<PathBuf, PathBuf>,
    /// Files that could not be read, with the error.
    pub unreadable: Vec<(PathBuf, String)>,
    /// Files parsed for this call (the others came from the cache).
    pub files_read: usize,
}

impl Classification {
    pub fn is_fragment(&self, path: &Path) -> bool {
        self.fragments.contains_key(&key(path))
    }

    /// A program that includes the fragment `path`.
    pub fn included_by(&self, path: &Path) -> Option<&Path> {
        self.fragments.get(&key(path)).map(PathBuf::as_path)
    }

    pub fn fragment_count(&self) -> usize {
        self.fragments.len()
    }
}

/// Modification time and size, or `None` when the path does not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp(Option<(Option<SystemTime>, u64)>);

fn stamp(path: &Path) -> Stamp {
    Stamp(std::fs::metadata(path).ok().map(|m| (m.modified().ok(), m.len())))
}

/// A cached value and the stamps of everything it was computed from.
struct Stamped<T> {
    stamps: Vec<(PathBuf, Stamp)>,
    value: T,
}

impl<T> Stamped<T> {
    fn is_current(&self) -> bool {
        self.stamps.iter().all(|(p, s)| stamp(p) == *s)
    }
}

type Cache<T> = Mutex<HashMap<PathBuf, Stamped<T>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn includes_cache() -> &'static Cache<Vec<PathBuf>> {
    static CACHE: OnceLock<Cache<Vec<PathBuf>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn summary_cache() -> &'static Cache<Summary> {
    static CACHE: OnceLock<Cache<Summary>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Forget every cached result.
pub fn clear_cache() {
    lock(includes_cache()).clear();
    lock(summary_cache()).clear();
}

/// How paths are compared: case-insensitively on Windows (an `#include` may differ in case from the name on
/// disk).
fn key(path: &Path) -> PathBuf {
    if cfg!(windows) { PathBuf::from(path.to_string_lossy().to_lowercase()) } else { path.to_path_buf() }
}

/// The SFZ files read and the includes not found, stamped (the root is stamped before it is read).
fn source_stamps(inst: &Instrument, root_stamp: Stamp) -> Vec<(PathBuf, Stamp)> {
    let mut stamps = vec![(inst.path().to_path_buf(), root_stamp)];
    stamps.extend(inst.files().iter().skip(1).map(|f| (f.path.clone(), stamp(&f.path))));
    for i in inst.includes().iter().filter(|i| i.target.is_none()) {
        let p = lexical_normalize(&inst.dir().join(&i.path));
        let s = stamp(&p);
        stamps.push((p, s));
    }
    stamps
}

/// The files `path` includes when it is loaded as a program; `true` when it came from the cache.
fn includes_of(path: &Path) -> Result<(Vec<PathBuf>, bool)> {
    if let Some(e) = lock(includes_cache()).get(path).filter(|e| e.is_current()) {
        return Ok((e.value.clone(), true));
    }
    let before = stamp(path);
    let inst = Instrument::load_with(path, &LoadOptions { check_samples: false, ..LoadOptions::default() })?;
    let included: Vec<PathBuf> = inst.files().iter().skip(1).map(|f| f.path.clone()).collect();
    let stamps = source_stamps(&inst, before);
    lock(includes_cache()).insert(path.to_path_buf(), Stamped { stamps, value: included.clone() });
    Ok((included, false))
}

/// The folder a file's library is: `<root>/instruments/<library>`, else the first folder under the root.
fn library(root: &Path, path: &Path) -> Option<PathBuf> {
    let rel = key(path).strip_prefix(key(root)).ok()?.to_path_buf();
    let mut parts = rel.components().filter(|c| matches!(c, Component::Normal(_)));
    let first = parts.next()?;
    Some(if first.as_os_str() == "instruments" { Path::new(first.as_os_str()).join(parts.next()?) } else { first.as_os_str().into() })
}

/// Tell programs from fragments among `files` (absolute SFZ paths). See the [module documentation](self).
pub fn classify(root: &Path, files: &[PathBuf]) -> Classification {
    let mut order: Vec<&PathBuf> = files.iter().collect();
    order.sort_by_cached_key(|p| (p.components().count(), key(p)));
    order.dedup_by_key(|p| key(p));
    let listed: HashSet<PathBuf> = order.iter().map(|p| key(p)).collect();
    let mut out = Classification::default();
    let mut includes: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut unreadable: BTreeSet<PathBuf> = BTreeSet::new();

    // what a program marks as fragments: listed files of its library, other than itself
    let marks = |program: &Path, included: &[PathBuf], into: &mut HashMap<PathBuf, PathBuf>| {
        let lib = library(root, program);
        for t in included {
            let k = key(t);
            if k != key(program) && listed.contains(&k) && library(root, t) == lib {
                into.entry(k).or_insert_with(|| program.to_path_buf());
            }
        }
    };
    let mut read = |p: &Path, out: &mut Classification, includes: &mut HashMap<PathBuf, Vec<PathBuf>>| -> Option<Vec<PathBuf>> {
        if let Some(i) = includes.get(&key(p)) {
            return Some(i.clone());
        }
        if unreadable.contains(&key(p)) {
            return None;
        }
        match includes_of(p) {
            Ok((i, cached)) => {
                out.files_read += usize::from(!cached);
                includes.insert(key(p), i.clone());
                Some(i)
            }
            Err(e) => {
                unreadable.insert(key(p));
                out.unreadable.push((p.to_path_buf(), e.to_string()));
                None
            }
        }
    };

    let mut fragments = HashMap::new();
    for p in &order {
        if fragments.contains_key(&key(p)) {
            continue;
        }
        if let Some(i) = read(p, &mut out, &mut includes) {
            marks(p, &i, &mut fragments);
        }
    }
    // settle: only what programs include counts (a fragment read as a program early on resolves its includes
    // from the wrong folder)
    for _ in 0..8 {
        let mut settled = HashMap::new();
        for p in order.iter().filter(|p| !fragments.contains_key(&key(p))) {
            if let Some(i) = read(p, &mut out, &mut includes) {
                marks(p, &i, &mut settled);
            }
        }
        let same = settled.len() == fragments.len() && settled.keys().all(|k| fragments.contains_key(k));
        fragments = settled;
        if same {
            break;
        }
    }
    out.fragments = fragments;
    out
}

/// How many keyswitches [`Summary::keyswitches`] lists.
const MAX_KEYSWITCHES: usize = 12;

/// Summarise the program at `path` (cached; see the [module documentation](self)).
pub fn summary(path: &Path) -> Result<Summary> {
    if let Some(e) = lock(summary_cache()).get(path).filter(|e| e.is_current()) {
        return Ok(e.value.clone());
    }
    let before = stamp(path);
    let inst = Instrument::load(path)?;
    let r = ranges(&inst);
    let paths = check_paths(&inst);
    let mut switches: Vec<String> = r
        .keyswitches
        .iter()
        .take(MAX_KEYSWITCHES)
        .map(|k| {
            let keys = match k.hi {
                Some(hi) if hi != k.key => format!("{}-{hi}", k.key),
                _ => k.key.to_string(),
            };
            match &k.label {
                Some(l) => format!("{keys} {l}"),
                None => keys,
            }
        })
        .collect();
    if r.keyswitches.len() > MAX_KEYSWITCHES {
        switches.push(format!("and {} more", r.keyswitches.len() - MAX_KEYSWITCHES));
    }
    let keys = match r.keys.as_slice() {
        [] => "none".to_string(),
        [one] => span_text(*one),
        [first, .., last] if r.keys.len() > 3 => {
            format!("{} with gaps ({} spans)", span_text(Span(first.0, last.1)), r.keys.len())
        }
        spans => spans_text(spans),
    };
    let s = Summary {
        regions: r.playable_regions,
        keys,
        keyswitches: switches.join(", "),
        velocity_layers: r.velocity_layers,
        missing_samples: paths.missing.len(),
        errors: inst.errors().count(),
    };
    let mut stamps = source_stamps(&inst, before);
    let dirs: BTreeSet<PathBuf> = inst.sample_files().iter().filter_map(|f| f.path.parent().map(Path::to_path_buf)).collect();
    stamps.extend(dirs.into_iter().map(|d| {
        let s = stamp(&d);
        (d, s)
    }));
    lock(summary_cache()).insert(path.to_path_buf(), Stamped { stamps, value: s.clone() });
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flux-sfz-catalog-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn fragments_are_what_programs_of_the_library_include() {
        let root = tmp("classify");
        let lib = root.join("instruments/lib");
        // includes are relative to the program's folder, even inside included files
        write(&lib.join("Programs/main.sfz"), "<control> default_path=../Samples/\n#include \"maps/map.sfz\"\n");
        write(&lib.join("Programs/maps/map.sfz"), "#include \"maps/inner.sfz\"\n<region> sample=a.wav\n");
        write(&lib.join("Programs/maps/inner.sfz"), "<region> sample=b.wav\n");
        // read as a program, inner.sfz would be maps/maps/inner.sfz: not found, so no false mark
        write(&lib.join("Programs/maps/maps/orphan.sfz"), "<region> sample=c.wav\n");
        // a whole program wrapped by a hand-made one elsewhere stays a program
        write(&lib.join("Solo.sfz"), "<region> sample=s.wav\n");
        write(&root.join("sfz/wrap.sfz"), "<global> volume=-3\n#include \"../instruments/lib/Solo.sfz\"\n");
        let files: Vec<PathBuf> = [
            "instruments/lib/Programs/main.sfz",
            "instruments/lib/Programs/maps/map.sfz",
            "instruments/lib/Programs/maps/inner.sfz",
            "instruments/lib/Programs/maps/maps/orphan.sfz",
            "instruments/lib/Solo.sfz",
            "sfz/wrap.sfz",
        ]
        .iter()
        .map(|f| lexical_normalize(&root.join(f)))
        .collect();
        let c = classify(&root, &files);
        let fragment: Vec<bool> = files.iter().map(|f| c.is_fragment(f)).collect();
        assert_eq!(fragment, [false, true, true, false, false, false]);
        assert_eq!(c.included_by(&files[2]), Some(files[0].as_path()));
        assert_eq!(c.files_read, 4);

        // warm: everything from the cache; a change is noticed
        let again = classify(&root, &files);
        assert_eq!(again.files_read, 0);
        assert_eq!(again.fragment_count(), 2);
        write(&lib.join("Programs/main.sfz"), "<control> default_path=../Samples/\n<region> sample=x.wav\n");
        let changed = classify(&root, &files);
        assert!(!changed.is_fragment(&files[1]) && !changed.is_fragment(&files[2]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summary_notices_new_samples() {
        let root = tmp("summary");
        let p = root.join("p.sfz");
        write(&p, "<group> sw_lokey=24 sw_hikey=25 sw_default=24\n<region> sw_last=24 sw_label=Sus sample=s/a.wav lokey=48 hikey=60\n\
                   <region> sw_last=25 sample=s/b.wav lokey=48 hikey=60\n");
        write(&root.join("s/a.wav"), "");
        let s = summary(&p).unwrap();
        assert_eq!((s.regions, s.missing_samples), (1, 1));
        assert_eq!(s.keyswitches, "24 Sus");
        assert_eq!(s.keys, "C3-C4 (48-60)");
        // the sample appears: its folder's stamp changes
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&root.join("s/b.wav"), "");
        let s = summary(&p).unwrap();
        assert_eq!((s.regions, s.missing_samples), (2, 0));
        assert_eq!(s.keyswitches, "24 Sus, 25");
        let _ = std::fs::remove_dir_all(&root);
    }
}
