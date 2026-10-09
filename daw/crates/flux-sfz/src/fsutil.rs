//! Path helpers: lexical normalisation, relative paths with `/`, and finding files whose name on disk differs
//! in case from the name written in an SFZ file (fine on Windows and macOS, missing on Linux).

use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Resolve `.` and `..` without touching the filesystem.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped = matches!(out.components().next_back(), Some(Component::Normal(_))) && out.pop();
                if !popped && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `to` relative to the directory `from`, with `/` separators (`../samples/x.wav`). `None` if there is no
/// relative path (different drives). Both paths should be absolute and normalised.
pub fn relative_path(from: &Path, to: &Path) -> Option<String> {
    let from: Vec<Component<'_>> = from.components().collect();
    let to_c: Vec<Component<'_>> = to.components().collect();
    // different drives
    if let (Some(a), Some(b)) = (from.first(), to_c.first())
        && (matches!(a, Component::Prefix(_)) || matches!(b, Component::Prefix(_)))
        && !same_component(a, b)
    {
        return None;
    }
    let common = from.iter().zip(&to_c).take_while(|(a, b)| same_component(a, b)).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(to_c[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    Some(parts.join("/"))
}

/// Path components compare case-insensitively on Windows.
fn same_component(a: &Component<'_>, b: &Component<'_>) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// `path` as text with `/` separators.
pub fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Whether a referenced file exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum FileStatus {
    Found,
    Missing,
    /// The file exists, but the name on disk differs in case (found here only because the filesystem ignores
    /// case; a case-sensitive system would not find it).
    CaseMismatch { on_disk: PathBuf },
    /// Not looked up ([`crate::LoadOptions::check_samples`] was off).
    Unchecked,
}

impl FileStatus {
    /// True unless the file is missing (case mismatches play on this machine).
    pub fn exists(&self) -> bool {
        !matches!(self, FileStatus::Missing)
    }
}

/// Looks files up, remembering directory listings so that checking thousands of samples stays cheap.
#[derive(Default)]
pub struct FileChecker {
    /// directory -> (lower-case name -> names on disk)
    dirs: HashMap<PathBuf, Option<HashMap<String, Vec<OsString>>>>,
    files: HashMap<PathBuf, FileStatus>,
}

impl FileChecker {
    pub fn new() -> FileChecker {
        FileChecker::default()
    }

    /// The status of a file given by an absolute, normalised path.
    pub fn status(&mut self, path: &Path) -> FileStatus {
        if let Some(s) = self.files.get(path) {
            return s.clone();
        }
        let s = self.lookup(path);
        self.files.insert(path.to_path_buf(), s.clone());
        s
    }

    fn lookup(&mut self, path: &Path) -> FileStatus {
        if !path.is_file() {
            return FileStatus::Missing;
        }
        let mut on_disk = PathBuf::new();
        let mut mismatch = false;
        for c in path.components() {
            let Component::Normal(name) = c else {
                on_disk.push(c.as_os_str());
                continue;
            };
            let listing = self.dirs.entry(on_disk.clone()).or_insert_with_key(|dir| list_dir(dir));
            let actual = listing.as_ref().and_then(|l| {
                let candidates = l.get(&name.to_string_lossy().to_lowercase())?;
                Some(candidates.iter().find(|n| n.as_os_str() == name).unwrap_or(&candidates[0]).clone())
            });
            match actual {
                Some(a) => {
                    mismatch |= a.as_os_str() != name;
                    on_disk.push(a);
                }
                // unreadable directory: trust the name as written
                None => on_disk.push(name),
            }
        }
        if mismatch { FileStatus::CaseMismatch { on_disk } } else { FileStatus::Found }
    }
}

fn list_dir(dir: &Path) -> Option<HashMap<String, Vec<OsString>>> {
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let mut map: HashMap<String, Vec<OsString>> = HashMap::new();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name();
        map.entry(name.to_string_lossy().to_lowercase()).or_default().push(name);
    }
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize() {
        assert_eq!(lexical_normalize(Path::new("a/./b/../c.wav")), PathBuf::from("a/c.wav"));
        assert_eq!(lexical_normalize(Path::new("../x/../y")), PathBuf::from("../y"));
        assert_eq!(lexical_normalize(Path::new("/a/../../b")), PathBuf::from("/b"));
    }

    #[test]
    fn relative() {
        let r = relative_path(Path::new("/r/build/sfz/set"), Path::new("/r/instruments/lib/Programs"));
        assert_eq!(r.as_deref(), Some("../../../instruments/lib/Programs"));
        assert_eq!(relative_path(Path::new("/r/a"), Path::new("/r/a")).as_deref(), Some(""));
        #[cfg(windows)]
        {
            assert_eq!(relative_path(Path::new("C:/a"), Path::new("D:/b")), None);
            assert_eq!(relative_path(Path::new("C:/A/x"), Path::new("c:/a/y")).as_deref(), Some("../y"));
        }
    }

    #[test]
    fn finds_case_mismatches() {
        let dir = std::env::temp_dir().join(format!("flux-sfz-case-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Sub")).unwrap();
        std::fs::write(dir.join("Sub").join("Note.wav"), b"x").unwrap();
        let mut fc = FileChecker::new();
        assert_eq!(fc.status(&dir.join("Sub").join("Note.wav")), FileStatus::Found);
        assert_eq!(fc.status(&dir.join("Sub").join("none.wav")), FileStatus::Missing);
        if cfg!(windows) {
            let s = fc.status(&dir.join("sub").join("note.WAV"));
            assert_eq!(s, FileStatus::CaseMismatch { on_disk: dir.join("Sub").join("Note.wav") });
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
