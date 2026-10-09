//! Standard folders that assets installed by another tool live in, so the manifest can say where such an asset is
//! without naming anyone's home folder: `{vst3}/Vital.vst3`, `{documents}/Vital`.
//!
//! * `{vst3}`: the VST3 folders (Windows `%COMMONPROGRAMFILES%/VST3` and `%LOCALAPPDATA%/Programs/Common/VST3`;
//!   macOS `/Library/Audio/Plug-Ins/VST3` and the user's; Linux `~/.vst3`, `/usr/lib/vst3`, `/usr/local/lib/vst3`).
//! * `{documents}`: the user's Documents (Windows also OneDrive's, when Documents is redirected there).
//! * `{appdata}`: per-user application data (`%APPDATA%`; `~/Library/Application Support`; `~/.config`).
//! * `{programdata}`: shared application data (`%PROGRAMDATA%`; `/Library/Application Support`; `/usr/share`,
//!   `/usr/local/share`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The placeholders an install location may start with.
pub const NAMES: [&str; 4] = ["vst3", "documents", "appdata", "programdata"];

/// The folders each placeholder stands for on one machine.
#[derive(Clone, Debug, Default)]
pub struct Places {
    dirs: BTreeMap<String, Vec<PathBuf>>,
}

impl Places {
    /// This machine's standard folders, from the environment.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var("HOME");
        let mut p = Places::default();
        if cfg!(windows) {
            p.dirs.insert(
                "vst3".into(),
                [var("COMMONPROGRAMFILES").map(|c| c.join("VST3")), var("LOCALAPPDATA").map(|l| l.join("Programs/Common/VST3"))]
                    .into_iter()
                    .flatten()
                    .collect(),
            );
            p.dirs.insert("documents".into(), ["USERPROFILE", "OneDrive"].iter().filter_map(|k| var(k)).map(|d| d.join("Documents")).collect());
            p.dirs.insert("appdata".into(), var("APPDATA").into_iter().collect());
            p.dirs.insert("programdata".into(), var("PROGRAMDATA").into_iter().collect());
        } else if cfg!(target_os = "macos") {
            let user = |rel: &str| home.iter().map(|h| h.join(rel)).collect::<Vec<_>>();
            p.dirs.insert("vst3".into(), [vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")], user("Library/Audio/Plug-Ins/VST3")].concat());
            p.dirs.insert("documents".into(), user("Documents"));
            p.dirs.insert("appdata".into(), user("Library/Application Support"));
            p.dirs.insert("programdata".into(), vec![PathBuf::from("/Library/Application Support")]);
        } else {
            let user = |rel: &str| home.iter().map(|h| h.join(rel)).collect::<Vec<_>>();
            p.dirs.insert("vst3".into(), [user(".vst3"), vec![PathBuf::from("/usr/lib/vst3"), PathBuf::from("/usr/local/lib/vst3")]].concat());
            p.dirs.insert("documents".into(), user("Documents"));
            p.dirs.insert("appdata".into(), var("XDG_CONFIG_HOME").map_or_else(|| user(".config"), |c| vec![c]));
            p.dirs.insert("programdata".into(), vec![PathBuf::from("/usr/share"), PathBuf::from("/usr/local/share")]);
        }
        p
    }

    /// Replace the folders of one placeholder (tests, or a machine with unusual folders).
    pub fn with(mut self, name: &str, dirs: Vec<PathBuf>) -> Self {
        self.dirs.insert(name.to_string(), dirs);
        self
    }

    /// Every path `location` (`{name}/rest`) can be at here, in the placeholder's folder order. Empty when the
    /// location is malformed or the placeholder stands for no folder on this machine.
    pub fn expand(&self, location: &str) -> Vec<PathBuf> {
        let Some((name, rest)) = split(location) else { return vec![] };
        self.dirs.get(name).map_or_else(Vec::new, |dirs| dirs.iter().map(|d| join(d, rest)).collect())
    }

    /// The first path `location` can be at that exists.
    pub fn find(&self, location: &str) -> Option<PathBuf> {
        self.expand(location).into_iter().find(|p| p.exists())
    }
}

/// Split `{name}/rest` (or a bare `{name}`) into the placeholder name and the rest.
pub fn split(location: &str) -> Option<(&str, &str)> {
    let (name, rest) = location.strip_prefix('{')?.split_once('}')?;
    match rest {
        "" => Some((name, "")),
        r => r.strip_prefix('/').map(|r| (name, r)),
    }
}

fn join(dir: &Path, rest: &str) -> PathBuf {
    rest.split('/').filter(|c| !c.is_empty()).fold(dir.to_path_buf(), |p, c| p.join(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_placeholders() {
        let p = Places::default().with("vst3", vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(p.expand("{vst3}/iZotope/Vinyl.vst3"), [PathBuf::from("/a/iZotope/Vinyl.vst3"), PathBuf::from("/b/iZotope/Vinyl.vst3")]);
        assert_eq!(p.expand("{vst3}"), [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert!(p.expand("{documents}/Vital").is_empty());
        assert!(p.expand("vst3/Vital.vst3").is_empty());
        assert_eq!(split("{vst3}x"), None);
        assert_eq!(split("{documents}/STL/Ignite"), Some(("documents", "STL/Ignite")));
    }

    #[test]
    fn every_placeholder_is_defined() {
        let p = Places::from_env();
        for n in NAMES {
            assert!(p.dirs.contains_key(n), "{n}");
        }
    }
}
