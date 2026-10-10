//! Flux's settings for this user, in `settings.json` in Flux's data folder (`%LOCALAPPDATA%\Flux` on Windows).
//! Setup and the Library window write it; Flux reads it at startup. Keys this build doesn't know are kept, so an
//! older Flux never drops what a newer one wrote.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The settings file's contents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The library folder (instruments, tools, SFZ mappings, instrument cards) of an installed Flux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<PathBuf>,
    /// Where projects and exports go (default `Documents\Flux`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documents: Option<PathBuf>,
    /// Look for a newer Flux on its releases page, at most once a day.
    #[serde(default = "yes")]
    pub check_updates: bool,
    /// When that last happened (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_check: Option<u64>,
    /// The newest release seen then (its tag), so the notice can show without asking again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_release: Option<String>,
    /// A release the user chose not to hear about again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_release: Option<String>,
    /// Keys written by other versions of Flux.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            library: None,
            documents: None,
            check_updates: true,
            last_update_check: None,
            latest_release: None,
            skipped_release: None,
            other: serde_json::Map::new(),
        }
    }
}

/// Flux's per-user data folder: `%LOCALAPPDATA%\Flux`, `~/Library/Application Support/Flux` or
/// `$XDG_DATA_HOME/flux` (`~/.local/share/flux`).
pub fn data_dir() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        var("LOCALAPPDATA").map(|d| d.join("Flux"))
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support/Flux"))
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share"))).map(|d| d.join("flux"))
    }
}

/// The settings file of this user.
pub fn path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("settings.json"))
}

impl Settings {
    /// This user's settings; defaults when there are none yet or the file can't be read.
    pub fn load() -> Settings {
        path().map(|p| Settings::load_from(&p)).unwrap_or_default()
    }

    /// Settings from a file; defaults when it is missing or unreadable (a broken file is reported, not fatal).
    pub fn load_from(path: &Path) -> Settings {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("flux: ignoring {} ({e})", path.display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    /// Write this user's settings.
    pub fn save(&self) -> Result<()> {
        self.save_to(&path().context("no folder for Flux's settings (LOCALAPPDATA is not set)")?)
    }

    /// Write settings to a file, replacing it in one step so a crash never leaves half a file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    /// Change this user's settings: read them fresh, apply `f`, write them back.
    pub fn update(f: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let p = path().context("no folder for Flux's settings (LOCALAPPDATA is not set)")?;
        let mut s = Settings::load_from(&p);
        f(&mut s);
        s.save_to(&p)?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Flux").join("settings.json");
        assert_eq!(Settings::load_from(&p), Settings::default());
        assert!(Settings::default().check_updates);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"library": "D:/Flux Library", "check_updates": false, "from_the_future": {"a": 1}}"#).unwrap();
        let mut s = Settings::load_from(&p);
        assert_eq!(s.library.as_deref(), Some(Path::new("D:/Flux Library")));
        assert!(!s.check_updates);
        s.skipped_release = Some("v0.2.0".into());
        s.save_to(&p).unwrap();
        let back: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(back["from_the_future"]["a"], 1);
        assert_eq!(back["skipped_release"], "v0.2.0");
        assert!(back.get("documents").is_none(), "unset keys are left out");
        // a broken file reads as the defaults
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
    }
}
